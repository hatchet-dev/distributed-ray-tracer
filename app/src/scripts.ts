// Shape scripts and reference images can be megabytes: too big to copy into every Hatchet task's
// input. Each is published once to its own durable stream topic (script-<id> or image-<id>), where
// the id is the SHA-256 of the bytes, and tasks carry the id. The tile Worker reads script topics
// too (crates/endpoint/src/scripts.rs). Both are capped (MAX_SCRIPT_BYTES, MAX_IMAGE_BYTES) to fit
// in one stream message.

import { env } from 'cloudflare:workers';
import { HatchetCore } from '@hatchet-dev/typescript-sdk/core';

type Kind = 'script' | 'image';
const topic = (kind: Kind, id: string) => `${kind}-${id}`;
const hatchet = () => new HatchetCore({ token: (env as unknown as { HATCHET_CLIENT_TOKEN: string }).HATCHET_CLIENT_TOKEN });

/** Publishes bytes (once per distinct content, while the topic retains them) and returns their id. */
async function put(kind: Kind, bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
  const id = [...digest].map((b) => b.toString(16).padStart(2, '0')).join('');
  const streams = hatchet().streams;
  // A topic nothing was published to (or whose message expired) has none: publish it.
  const stored = await streams.topicMetadata(topic(kind, id)).then((m) => m.messageCount > 0, () => false);
  if (!stored) await streams.publish(topic(kind, id), bytes);
  return id;
}

/** The bytes published under `id`. */
async function get(kind: Kind, id: string): Promise<Uint8Array> {
  const streams = hatchet().streams;
  // A subscription to an empty topic waits for a message forever: check there is one first.
  const stored = await streams.topicMetadata(topic(kind, id)).then((m) => m.messageCount > 0, () => false);
  if (!stored) throw new Error(`${kind} ${id} is not stored (or has expired)`);
  const reading = new AbortController();
  try {
    for await (const event of streams.subscribe(topic(kind, id), { signal: reading.signal })) {
      return event.payload;
    }
  } finally {
    reading.abort(); // hang up: the first message is the content
  }
  throw new Error(`${kind} ${id} is not stored (or has expired)`);
}

export const putScript = (script: string) => put('script', new TextEncoder().encode(script));
export const getScript = async (id: string) => new TextDecoder().decode(await get('script', id));

/** Image formats Claude reads. */
export type ImageType = 'image/png' | 'image/jpeg' | 'image/webp' | 'image/gif';
/** Under the ~4 MiB of one stream message. The page shrinks images well below this before uploading. */
export const MAX_IMAGE_BYTES = 3.5 * 1024 * 1024;
export const MAX_IMAGES = 6;

/** The image's format, from its first bytes; null if it isn't one Claude reads. */
export function imageType(b: Uint8Array): ImageType | null {
  const starts = (...sig: number[]) => sig.every((v, i) => b[i] === v);
  if (starts(0x89, 0x50, 0x4e, 0x47)) return 'image/png';
  if (starts(0xff, 0xd8, 0xff)) return 'image/jpeg';
  if (starts(0x47, 0x49, 0x46, 0x38)) return 'image/gif';
  if (starts(0x52, 0x49, 0x46, 0x46) && b[8] === 0x57 && b[9] === 0x45 && b[10] === 0x42 && b[11] === 0x50) return 'image/webp';
  return null;
}

export const putImage = (bytes: Uint8Array<ArrayBuffer>) => put('image', bytes);

/** A stored image, base64-encoded for the Messages API. */
export async function getImage(id: string): Promise<{ mediaType: ImageType; data: string }> {
  const bytes = await get('image', id);
  const mediaType = imageType(bytes);
  if (!mediaType) throw new Error(`image ${id} is not a supported image`);
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return { mediaType, data: btoa(binary) };
}
