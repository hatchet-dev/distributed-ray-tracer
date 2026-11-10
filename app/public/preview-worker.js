// Runs the preview module (crates/preview, built into ./preview/) off the main thread: each
// message is a script; the reply is its parts' meshes and metadata. Scripts that arrive while
// one is being meshed replace each other, so only the latest is meshed next.
import init, { preview } from './preview/preview.js';

const ready = init();
let pending = null, busy = false;

onmessage = (e) => {
  pending = e.data;
  if (!busy) run();
};

async function run() {
  busy = true;
  await ready;
  while (pending) {
    const { id, script } = pending;
    pending = null;
    const started = performance.now();
    let p;
    try {
      p = preview(script, 0);
      const meta = JSON.parse(p.meta());
      const meshes = (meta.parts || []).map((_, i) => ({
        positions: p.positions(i), normals: p.normals(i), colors: p.colors(i), indices: p.indices(i),
      }));
      const buffers = meshes.flatMap((m) => [m.positions.buffer, m.normals.buffer, m.colors.buffer, m.indices.buffer]);
      postMessage({ id, meta, meshes, ms: performance.now() - started }, buffers);
    } catch (err) {
      // A panic leaves the wasm instance unusable: the page starts a fresh worker.
      postMessage({ id, crashed: String(err) });
      return;
    } finally {
      p?.free();
    }
  }
  busy = false;
}
