// Shape scripts can be megabytes: too big to copy into every Hatchet task's input. Each is
// published once to its own durable stream topic, script-<id>, where the id is the SHA-256 of
// the text, and tasks carry the id. The tile Worker reads the same topic (crates/endpoint/src/scripts.rs).
// A script is up to MAX_SCRIPT_BYTES (jobs.ts), so it fits in one stream message.

import { env } from 'cloudflare:workers';
import { HatchetCore } from '@hatchet-dev/typescript-sdk/core';

const topic = (id: string) => `script-${id}`;
const hatchet = () => new HatchetCore({ token: (env as unknown as { HATCHET_CLIENT_TOKEN: string }).HATCHET_CLIENT_TOKEN });

/** Publishes a script (once per distinct text, while the topic retains it) and returns its id. */
export async function putScript(script: string): Promise<string> {
  const bytes = new TextEncoder().encode(script);
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
  const id = [...digest].map((b) => b.toString(16).padStart(2, '0')).join('');
  const streams = hatchet().streams;
  // A topic nothing was published to (or whose message expired) has none: publish it.
  const stored = await streams.topicMetadata(topic(id)).then((m) => m.messageCount > 0, () => false);
  if (!stored) await streams.publish(topic(id), bytes);
  return id;
}

/** The script published under `id`. */
export async function getScript(id: string): Promise<string> {
  const reading = new AbortController();
  try {
    for await (const event of hatchet().streams.subscribe(topic(id), { signal: reading.signal })) {
      return new TextDecoder().decode(event.payload);
    }
  } finally {
    reading.abort(); // hang up: the first message is the script
  }
  throw new Error(`script ${id} is not stored (or has expired)`);
}
