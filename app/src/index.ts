// The app as one Cloudflare Worker:
//
//   /hatchet/*         Hatchet's serverless endpoint for the orchestration workflows (workflows.ts)
//   POST /api/render   starts a render and streams its progress back as Server-Sent Events
//   everything else    the page (static assets, public/)

import { cloudflare, type HatchetEnv } from '@hatchet-dev/serverless/cloudflare';
import { HatchetCore } from '@hatchet-dev/typescript-sdk/core';
import { MAX_SCRIPT_BYTES, parseJob, workflowFor, type Job } from './jobs';
import { putScript } from './scripts';
import { describe, type StreamMessage } from './messages';
import { workflows } from './workflows';

interface Env extends HatchetEnv {
  HATCHET_CLIENT_TOKEN: string;
}

export default cloudflare<Env>({
  workflows,
  fetch: async (req, env, ctx) => {
    const url = new URL(req.url);
    if (req.method === 'POST' && url.pathname === '/api/render') {
      const raw = await req.text();
      if (raw.length > 2 * MAX_SCRIPT_BYTES) return new Response('request too large', { status: 413 });
      let job: Job | string;
      try {
        job = parseJob(JSON.parse(raw));
      } catch (err) {
        job = `invalid request: ${describe(err)}`;
      }
      if (typeof job === 'string') return new Response(job, { status: 400 });
      return render(job, env, ctx, req.signal);
    }
    return new Response('Not Found', { status: 404 });
  },
});

/** The job as its workflow's input: scripts are stored, and passed by id. */
async function workflowInput(job: Job) {
  if ('description' in job) {
    const { script, ...rest } = job;
    return script === undefined ? rest : { ...rest, scriptId: await putScript(script) };
  }
  const { shape, ...rest } = job;
  return { ...rest, scriptId: await putScript(shape) };
}

/**
 * Starts the job's workflow and relays its durable stream topic as Server-Sent Events until a
 * `done` or `error` message. If the browser disconnects first, the run and everything it spawned
 * are cancelled; they share a `render_id` tag, since run metadata propagates to children.
 */
function render(job: Job, env: Env, ctx: ExecutionContext, disconnected: AbortSignal): Response {
  const hatchet = new HatchetCore({ token: env.HATCHET_CLIENT_TOKEN });
  const renderId = crypto.randomUUID();
  const topic = `render-${renderId}`;
  const since = new Date(Date.now() - 60_000);
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
  const cancel = () => {
    if (finished) return;
    end();
    ctx.waitUntil(
      hatchet.runs
        .cancel({ filters: { since, additionalMetadata: { render_id: renderId } } })
        .then(() => undefined)
        .catch((err) => console.error(`failed to cancel render ${renderId}`, err))
    );
  };
  // A failed write also means the browser went away; the heartbeat makes sure one is tried.
  const write = (text: string) => {
    if (!finished) writer.write(encoder.encode(text)).catch(cancel);
  };
  const send = (message: StreamMessage | { type: 'start'; runId: string }) =>
    write(`event: ${message.type}\ndata: ${JSON.stringify(message)}\n\n`);
  const finish = (message: StreamMessage) => {
    if (finished) return;
    send(message);
    end();
    writer.close().catch(() => {});
  };
  const heartbeat = setInterval(() => write(': ping\n\n'), 15_000);
  disconnected.addEventListener('abort', cancel);

  ctx.waitUntil(
    (async () => {
      try {
        const opts = { additionalMetadata: { render_id: renderId } };
        const ref = await hatchet.runNoWait(workflowFor(job), { ...(await workflowInput(job)), topic }, opts);
        send({ type: 'start', runId: await ref.getWorkflowRunId() });
        // Safety net: a run that dies without publishing `error` still ends the stream.
        ref.output.catch((err) => setTimeout(() => finish({ type: 'error', message: describe(err) }), 5000));

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

  return new Response(readable, { headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' } });
}
