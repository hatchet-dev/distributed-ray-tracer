// The render API, independent of where it runs (cloudflare.ts and node.ts serve it, along with
// Hatchet's serverless endpoint for the workflows under /hatchet/* and the page in public/):
//
//   POST /api/images   stores a reference image
//   POST /api/render   starts a render and streams its progress back as Server-Sent Events
//   GET  /api/render/<id>?run=<runId>   replays a render's stream (the page resumes after a reload)
//   POST /api/render/<id>/cancel        cancels a render

import { HatchetCore } from '@hatchet-dev/typescript-sdk/core';
import { MAX_SCRIPT_BYTES, parseJob, workflowFor, type Job } from './jobs';
import { imageType, MAX_IMAGE_BYTES, putImage, putScript } from './scripts';
import { describe, type StreamMessage } from './messages';
import { secret, type Platform } from './env';
import { guardRender, pageConfig } from './guard';

/** Answers an API request, or null when the path isn't part of the API. */
export async function handleApi(req: Request, ctx: Platform): Promise<Response | null> {
  const url = new URL(req.url);
  if (req.method === 'GET' && url.pathname === '/api/config') return pageConfig();
  if (req.method === 'POST' && url.pathname === '/api/images') return uploadImage(req);
  if (req.method === 'POST' && url.pathname === '/api/render') {
    const raw = await req.text();
    if (raw.length > 2 * MAX_SCRIPT_BYTES) return new Response('request too large', { status: 413 });
    let job: Job | string;
    let body: Record<string, unknown> = {};
    let sessionId: string | undefined;
    try {
      body = JSON.parse(raw);
      job = parseJob(body);
      // The browser's session (index.html): render-scene runs one render per session at a time.
      if (typeof body?.sessionId === 'string' && UUID.test(body.sessionId)) sessionId = body.sessionId;
    } catch (err) {
      job = `invalid request: ${describe(err)}`;
    }
    if (typeof job === 'string') return new Response(job, { status: 400 });
    const refused = await guardRender(req, body, 'description' in job, ctx);
    if (refused) return refused;
    return render(job, ctx, req.signal, sessionId);
  }
  // /api/render/<renderId>: resume (GET) or cancel (POST .../cancel) a render.
  const m = url.pathname.match(/^\/api\/render\/([^/]+)(\/cancel)?$/);
  if (m && req.method === 'GET' && !m[2]) return resume(m[1], url.searchParams.get('run'), ctx, req.signal);
  if (m && req.method === 'POST' && m[2]) {
    if (!UUID.test(m[1])) return new Response('unknown render', { status: 400 });
    await cancelRender(hatchetFor(), m[1]);
    return new Response(null, { status: 204 });
  }
  return null;
}

/** POST /api/images: stores a reference image (the raw bytes) and returns `{ id }` for a chat turn's `imageIds`. */
async function uploadImage(req: Request): Promise<Response> {
  if (Number(req.headers.get('Content-Length') ?? 0) > MAX_IMAGE_BYTES) return tooLarge();
  const bytes = new Uint8Array(await req.arrayBuffer());
  if (bytes.length > MAX_IMAGE_BYTES) return tooLarge();
  if (!imageType(bytes)) return new Response('upload a PNG, JPEG, WebP or GIF image', { status: 415 });
  return Response.json({ id: await putImage(bytes) });
}
const tooLarge = () => new Response(`images must be under ${MAX_IMAGE_BYTES} bytes`, { status: 413 });

/** The job as its workflow's input: scripts are stored, and passed by id. */
async function workflowInput(job: Job) {
  if ('description' in job) {
    const { script, ...rest } = job;
    return script === undefined ? rest : { ...rest, scriptId: await putScript(script) };
  }
  const { shape, ...rest } = job;
  return { ...rest, scriptId: await putScript(shape) };
}

const SSE_HEADERS = { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' };
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
/** Renders run at most an hour (their execution timeout); cancelling looks back a little further. */
const RENDER_LOOKBACK_MS = 2 * 60 * 60 * 1000;

const hatchetFor = () => {
  const token = secret('HATCHET_CLIENT_TOKEN');
  if (!token) throw new Error('HATCHET_CLIENT_TOKEN is not set');
  return new HatchetCore({ token });
};

/** Cancels a render and everything it spawned: they share a `render_id` tag, since run metadata propagates to children. */
function cancelRender(hatchet: HatchetCore, renderId: string) {
  const since = new Date(Date.now() - RENDER_LOOKBACK_MS);
  return hatchet.runs
    .cancel({ filters: { since, additionalMetadata: { render_id: renderId } } })
    .then(() => undefined)
    .catch((err) => console.error(`failed to cancel render ${renderId}`, err));
}

/**
 * POST /api/render: starts the job's workflow and relays its stream (see `relay`). The first event,
 * `start`, carries the ids the page needs to resume or cancel the render later. A render keeps going
 * when the browser goes away (the page can come back to it), except before `start` was sent, when
 * nobody could cancel it: then it is cancelled.
 */
function render(job: Job, ctx: Platform, disconnected: AbortSignal, sessionId?: string): Response {
  const hatchet = hatchetFor();
  const renderId = crypto.randomUUID();
  let started = false;
  disconnected.addEventListener('abort', () => {
    if (!started) ctx.waitUntil(cancelRender(hatchet, renderId));
  });
  return relay(hatchet, renderId, ctx, disconnected, async (send) => {
    // Metadata carries over to child runs: cancelling keys on render_id, and render-scene's
    // concurrency (one render per session) on session_id. A request
    // without a session (e.g. from curl) is its own session.
    const opts = { additionalMetadata: { render_id: renderId, session_id: sessionId ?? renderId } };
    const ref = await hatchet.runNoWait(workflowFor(job), { ...(await workflowInput(job)), topic: `render-${renderId}` }, opts);
    const runId = await ref.getWorkflowRunId();
    started = !disconnected.aborted;
    if (!started) await cancelRender(hatchet, renderId);
    send({ type: 'start', renderId, runId });
    return runId;
  });
}

/** GET /api/render/<renderId>?run=<runId>: replays a render's stream from its start, then follows it live. */
async function resume(renderId: string, runId: string | null, ctx: Platform, disconnected: AbortSignal) {
  if (!UUID.test(renderId) || !runId || !UUID.test(runId)) return new Response('unknown render', { status: 400 });
  const hatchet = hatchetFor();
  // Subscribing to an empty topic would wait forever.
  const stored = await hatchet.streams.topicMetadata(`render-${renderId}`).then((m) => m.messageCount > 0, () => false);
  if (!stored) return new Response('this render is no longer stored', { status: 404 });
  return relay(hatchet, renderId, ctx, disconnected, async (send) => {
    send({ type: 'start', renderId, runId });
    return runId;
  });
}

type Start = { type: 'start'; renderId: string; runId: string };

/**
 * Relays a render's durable stream topic as Server-Sent Events, from its first message, until a
 * `done` or `error` message or the browser disconnects. `begin` sends `start` and returns the run
 * to watch: a run that dies without publishing `error` still ends the stream.
 */
function relay(
  hatchet: HatchetCore,
  renderId: string,
  ctx: Platform,
  disconnected: AbortSignal,
  begin: (send: (m: Start) => void) => Promise<string>
): Response {
  const topic = `render-${renderId}`;
  const reading = new AbortController();
  const encoder = new TextEncoder();
  const { readable, writable } = new TransformStream<Uint8Array, Uint8Array>();
  const writer = writable.getWriter();
  let finished = false;

  const end = () => {
    finished = true;
    clearInterval(heartbeat);
    reading.abort();
  };
  // A failed write means the browser went away; the heartbeat makes sure one is tried.
  const write = (text: string) => {
    if (!finished) writer.write(encoder.encode(text)).catch(end);
  };
  const send = (message: StreamMessage | Start) => write(`event: ${message.type}\ndata: ${JSON.stringify(message)}\n\n`);
  const finish = (message: StreamMessage) => {
    if (finished) return;
    send(message);
    end();
    writer.close().catch(() => {});
  };
  const heartbeat = setInterval(() => write(': ping\n\n'), 15_000);
  disconnected.addEventListener('abort', end);

  ctx.waitUntil(
    (async () => {
      try {
        const runId = await begin(send);
        // A description run hands the render off to its own run (`renderRunId`); watch that too.
        const watch = (run: { output: Promise<unknown> }) =>
          run.output.then(
            (out) => {
              // A run's output is keyed by task name.
              type Out = { renderRunId?: string };
              const o = out as (Out & Record<string, Out | undefined>) | undefined;
              const next = o?.['describe-and-render']?.renderRunId ?? o?.renderRunId;
              if (next) watch(hatchet.runRef(next));
            },
            (err) => setTimeout(() => finish({ type: 'error', message: describe(err) }), 5000)
          );
        watch(hatchet.runRef(runId));

        // `subscribe` ends when the engine hangs up an idle subscription; resume from the cursor.
        let cursor: string | undefined;
        while (!finished) {
          for await (const event of hatchet.streams.subscribe(topic, { cursor, signal: reading.signal })) {
            cursor = event.cursor;
            const message = JSON.parse(new TextDecoder().decode(event.payload)) as StreamMessage;
            if (message.type === 'done' || message.type === 'error') return finish(message);
            send(message);
          }
        }
      } catch (err) {
        finish({ type: 'error', message: describe(err) });
      }
    })()
  );

  return new Response(readable, { headers: SSE_HEADERS });
}
