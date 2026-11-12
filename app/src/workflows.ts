import { clientFromContext, hatchet, TaskRunTerminatedError, type DurableContext } from '@hatchet-dev/serverless';
import { ScriptWriter } from './describe';
import { getImage, getScript, putScript } from './scripts';
import { secret } from './env';
import {
  describe,
  type Camera,
  type DescribeInput,
  type RenderSceneInput,
  type StreamMessage,
  type Validation,
} from './messages';

/** Served by the Rust endpoint (crates/endpoint). */
const RENDER_TILES = 'render-tiles';
const VALIDATE_SHAPE = 'validate-shape';
/**
 * Render tasks per render: each renders on one core, so about the tile server's total cores.
 * RENDER_TASKS (a var on the app) overrides it.
 */
const RENDER_TASKS = 44;
/**
 * Most image bytes one task publishes (its tiles' packed RGB; base64 adds a third): a task's
 * tiles go to the render's stream as one message, under the engine's 4 MiB limit, so big images
 * get more tasks.
 */
const MAX_TASK_BYTES = 1_500_000;
/** Description -> script attempts before giving up (the first plus repairs). */
const SCRIPT_ATTEMPTS = 3;
/**
 * Default work budget per tile, in pixels x samples x (bounces + 1). Ray-marching a Fidget shape
 * costs ~2.4 us per unit in the tile Worker's wasm (measured with the gyroid example), so this
 * keeps a typical tile around 25 s of CPU, well inside that Worker's 300 s limit and the
 * endpoint's 300 s request timeout. More complex scripts cost proportionally more. The native
 * tile server (crates/server) is several times faster, so it can take bigger tiles:
 * TILE_WORK and TILE_MAX_SIZE (vars on the app) override these.
 */
const WORK_PER_TILE = 10_000_000;
/**
 * Small tiles: they're dealt round-robin to the render tasks, so smaller tiles even out the
 * tasks' work better (and the image fills in more finely as tasks finish).
 */
const MAX_TILE_SIZE = 16;
const numberVar = (name: string, fallback: number) => {
  const v = Number(secret(name));
  return Number.isFinite(v) && v > 0 ? v : fallback;
};
/** Matches `CameraParams::default()` in crates/scene. */
const DEFAULT_FOV = 39.6;

type Tile = { x: number; y: number; w: number; h: number };
type Bounds = NonNullable<Validation['bounds']>;
/** A workflow run's output is keyed by task name; a tiles task reports how many it published. */
type TilesRunOutput = { [RENDER_TILES]: { tiles: number } };
type Ctx = Parameters<typeof clientFromContext>[0];

const publish = (ctx: Ctx, topic: string, msg: StreamMessage) =>
  clientFromContext(ctx).streams.publish(topic, JSON.stringify(msg));

/** Parses a stored script on the Rust endpoint; rejects with Fidget's error if it doesn't parse. */
async function validate(ctx: Ctx, scriptId: string): Promise<Validation> {
  try {
    const out = await ctx.runChild<{ scriptId: string }, { [VALIDATE_SHAPE]: Validation }>(VALIDATE_SHAPE, { scriptId });
    return out[VALIDATE_SHAPE];
  } catch (err) {
    throw new Error(describe(err));
  }
}

/** `validate` for a durable task, whose children go through the durable log. */
async function validateDurable(ctx: DurableContext<RenderSceneInput>, scriptId: string): Promise<Validation> {
  try {
    const out = await ctx.spawnChild<{ scriptId: string }, { [VALIDATE_SHAPE]: Validation }>(VALIDATE_SHAPE, { scriptId });
    return out[VALIDATE_SHAPE];
  } catch (err) {
    if (err instanceof TaskRunTerminatedError) throw err;
    throw new Error(describe(err));
  }
}

/** Square tile side in pixels for these settings: a multiple of 8 between 8 and TILE_MAX_SIZE (16). */
export function tileSize({ samples, bounces }: { samples: number; bounces: number }) {
  const side = Math.sqrt(numberVar('TILE_WORK', WORK_PER_TILE) / (samples * (bounces + 1)));
  return Math.min(numberVar('TILE_MAX_SIZE', MAX_TILE_SIZE), Math.max(8, Math.floor(side / 8) * 8));
}

/** Matches `CameraParams::default()` in crates/scene. */
const DEFAULT_AZIMUTH = -60;
const DEFAULT_ELEVATION = 18;

/**
 * Aim the orbit camera at the shape, keeping the user's orbit angles: target the bounding box's
 * center, then pull back until all 8 corners fit inside both the horizontal and the vertical
 * field of view (rpt's fov spans the image's longer side).
 */
function frame(camera: Camera, bounds: Bounds, width: number, height: number): Camera {
  const center = [0, 1, 2].map((i) => (bounds.min[i] + bounds.max[i]) / 2) as [number, number, number];
  const rad = Math.PI / 180;
  const az = (camera.azimuth ?? DEFAULT_AZIMUTH) * rad;
  const el = (camera.elevation ?? DEFAULT_ELEVATION) * rad;
  // Camera basis: forward looks from the eye toward the target; Z is up.
  const back = [Math.cos(el) * Math.cos(az), Math.cos(el) * Math.sin(az), Math.sin(el)];
  const fwd = back.map((v) => -v);
  const rightRaw = [fwd[1], -fwd[0], 0]; // fwd × Z
  const rn = Math.hypot(...rightRaw) || 1;
  const right = rightRaw.map((v) => v / rn);
  const up = [right[1] * fwd[2] - right[2] * fwd[1], right[2] * fwd[0] - right[0] * fwd[2], right[0] * fwd[1] - right[1] * fwd[0]];
  const dot = (a: number[], b: number[]) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];

  const tanLong = Math.tan(((camera.fov ?? DEFAULT_FOV) * rad) / 2);
  const tanShort = (tanLong * Math.min(width, height)) / Math.max(width, height);
  const [tanX, tanY] = width >= height ? [tanLong, tanShort] : [tanShort, tanLong];

  // A corner p (relative to the target) is in view at distance d when
  // |p·right| <= tanX (d - p·fwd) and |p·up| <= tanY (d - p·fwd).
  let distance = 0;
  for (let corner = 0; corner < 8; corner++) {
    const p = [0, 1, 2].map((i) => ((corner >> i) & 1 ? bounds.max[i] : bounds.min[i]) - center[i]);
    const depth = dot(p, fwd);
    distance = Math.max(distance, depth + Math.abs(dot(p, right)) / tanX, depth + Math.abs(dot(p, up)) / tanY);
  }
  return { ...camera, target: center, distance: Math.min(60, Math.max(1, distance * 1.05)) };
}

/**
 * Run `fn`, publishing `error` if it fails, and `done` if it succeeds and `done` is set. A durable
 * task's eviction isn't a failure (the engine runs it again once its children finish), so it
 * publishes nothing.
 */
async function finishing<T>(ctx: Ctx, topic: string, done: boolean, fn: () => Promise<T>): Promise<T> {
  try {
    const out = await fn();
    if (done) await publish(ctx, topic, { type: 'done' });
    return out;
  } catch (err) {
    if (!(err instanceof TaskRunTerminatedError && err.reason === 'evicted')) {
      await publish(ctx, topic, { type: 'error', message: describe(err) }).catch(() => {});
    }
    throw err;
  }
}

/**
 * Validates the script (and frames the camera on the shape if asked), then fans out the tiles to
 * a few `render-tiles` children on the Rust endpoint. Each child publishes its tiles to the
 * render's durable stream itself, as one `tiles` message, so the page fills in as they finish.
 *
 * A durable task: while its children render it gives up its Worker invocation (after the
 * endpoint's inline wait budget) and the engine runs it again from the top when they're done,
 * replaying the children's results from its durable log. A render can take minutes, and
 * Cloudflare sheds Worker requests held that long when renders overlap; a durable task holds
 * none. Everything before the wait runs again on that replay, so the messages it publishes go
 * out only on the first invocation.
 */
export const renderScene = hatchet.durableTask({
  name: 'render-scene',
  executionTimeout: '60m',
  fn: async (input: RenderSceneInput, ctx: DurableContext<RenderSceneInput>) =>
    finishing(ctx, input.topic, true, async () => {
      const { topic, frame: shouldFrame, checked, scriptId, ...params } = input;
      const first = ctx.invocationCount <= 1;
      const say = (msg: StreamMessage) => (first ? publish(ctx, topic, msg) : undefined);

      // Parse the script once up front, so a typo fails here instead of in every tile.
      let validation = checked;
      if (!validation) {
        await say({ type: 'status', message: 'Checking the shape script…' });
        validation = await validateDurable(ctx, scriptId);
      }
      const { bounds } = validation;
      const scriptCamera = validation.camera ?? {};
      if (!bounds) throw new Error('The shape is empty above the floor (z = 0), so there is nothing to render');
      if (shouldFrame) {
        // A new script: take its chosen angle, field of view and depth of field, then aim at the
        // shape and pull back to fit it, unless the script pinned the target or distance itself.
        const { target, distance, ...angles } = scriptCamera;
        const framed = frame({ ...params.camera, ...angles }, bounds, params.width, params.height);
        params.camera = { ...framed, ...(target ? { target } : {}), ...(distance ? { distance } : {}) };
        await say({ type: 'camera', camera: params.camera });
      }

      const size = tileSize(params);
      const tiles: Tile[] = [];
      for (let y = 0; y < params.height; y += size)
        for (let x = 0; x < params.width; x += size)
          tiles.push({ x, y, w: Math.min(size, params.width - x), h: Math.min(size, params.height - y) });
      await say({ type: 'render', tiles: tiles.length, tileSize: size });
      await say({ type: 'status', message: `Rendering ${tiles.length} tiles…` });

      // A few tasks, each with tiles spread over the image (task k takes every n-th tile from
      // the k-th), so they get similar mixes of cheap and costly areas and finish together.
      const bytes = params.width * params.height * 3;
      const n = Math.min(tiles.length, Math.max(numberVar('RENDER_TASKS', RENDER_TASKS), Math.ceil(bytes / MAX_TASK_BYTES)));
      const tasks = Array.from({ length: n }, (_, k) => tiles.filter((_, i) => i % n === k));

      // Cancelling the render cancels its children too: the app cancels every run tagged with
      // the render's id. A child that fails (after its retries) fails the render.
      let outs: TilesRunOutput[];
      try {
        outs = await ctx.spawnChildren<typeof params & { tiles: Tile[]; scriptId: string; topic: string }, TilesRunOutput>(
          tasks.map((part) => ({ workflow: RENDER_TILES, input: { ...params, tiles: part, scriptId, topic } }))
        );
      } catch (err) {
        if (err instanceof TaskRunTerminatedError) throw err;
        throw new Error(`a render task failed: ${describe(err)}`);
      }
      const missing = outs.filter((out, k) => out?.[RENDER_TILES]?.tiles !== tasks[k].length).length;
      if (missing) throw new Error(`${missing} of ${n} render tasks finished without all their tiles`);
      return { tiles: tiles.length };
    }),
});

/**
 * Turns a plain-language description into a shape script with a language model, checks it with
 * the endpoint's `validate-shape` task (feeding parse errors back to the model to repair), then
 * starts `render-scene` as a child workflow, framed on the new shape, and returns without waiting
 * for it. Everything streams to one durable topic.
 */
export const describeAndRender = hatchet.task({
  name: 'describe-and-render',
  executionTimeout: '60m',
  fn: async (input: DescribeInput, ctx) =>
    // The render finishes the stream (render-scene publishes `done`); this only reports failures.
    finishing(ctx, input.topic, false, async () => {
      const { topic, description, model, effort, scriptId: currentId, history, imageIds = [], frame: shouldFrame = true, ...settings } = input;
      const current = currentId === undefined ? undefined : await getScript(currentId);
      const images = await Promise.all(imageIds.map(getImage));
      const writer = new ScriptWriter({ description, script: current, history, images }, model, effort);
      const verb = current === undefined ? 'Generating' : 'Updating';

      let script = '';
      let scriptId = '';
      let checked: Validation;
      let lastError = '';
      for (let attempt = 1; ; attempt++) {
        await publish(ctx, topic, {
          type: 'status',
          message: attempt === 1 ? `${verb} shape script…` : `Fixing shape script (attempt ${attempt})…`,
        });
        // Forward the model's text as it streams, so the editor fills in live.
        let pending = Promise.resolve();
        const onText = (text: string, restart: boolean) => {
          pending = pending.then(() => publish(ctx, topic, { type: 'script-delta', attempt, text, restart }));
        };
        const onSearch = () => {
          pending = pending.then(() => publish(ctx, topic, { type: 'status', message: 'Searching the web…' }));
        };
        const progress = { text: onText, search: onSearch };
        script = attempt === 1 ? await writer.write(progress) : await writer.repair(lastError, progress);
        await pending;
        await publish(ctx, topic, { type: 'script', attempt, script });

        await publish(ctx, topic, { type: 'status', message: 'Checking the shape script…' });
        try {
          scriptId = await putScript(script);
          checked = await validate(ctx, scriptId);
          break;
        } catch (err) {
          lastError = describe(err);
          if (attempt >= SCRIPT_ATTEMPTS) {
            throw new Error(`The generated script still didn't parse after ${attempt} attempts: ${lastError}`);
          }
          await publish(ctx, topic, { type: 'status', message: `The script didn't parse (${lastError}); fixing it…` });
        }
      }

      // A refinement keeps the viewer's camera, unless it changed the camera the script sets.
      const cameraOf = (v: Validation | null) => JSON.stringify(v?.camera ?? {});
      const before = currentId === undefined ? null : await validate(ctx, currentId).catch(() => null);
      const reframe = shouldFrame || cameraOf(before) !== cameraOf(checked);
      // Hand the render off instead of waiting on it: a long-lived task can lose its connection
      // (Cloudflare drops it with code 1006), and this one would otherwise be held open through
      // the model call and the whole render. The render API watches the returned run instead.
      const render = await ctx.runNoWaitChild(renderScene, { ...settings, scriptId, topic, frame: reframe, checked });
      return { script, renderRunId: await render.getWorkflowRunId() };
    }),
});

export const workflows = [renderScene, describeAndRender];
