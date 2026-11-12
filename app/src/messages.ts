// Types shared by the workflows (workflows.ts) and the render API (app.ts).

export type RenderSettings = {
  width: number;
  height: number;
  /** Most paths traced per pixel; the tracer stops early once a pixel converges. */
  samples: number;
  bounces: number;
  /** Orbit camera, see `CameraParams` in crates/scene. Omitted fields use the defaults. */
  camera: Camera;
};

export type Camera = Partial<{
  target: [number, number, number];
  azimuth: number;
  elevation: number;
  distance: number;
  fov: number;
  aperture: number;
}>;

export type RenderSceneInput = RenderSettings & {
  /** The Fidget (Rhai) script to render, by its id in the script store (scripts.ts). */
  scriptId: string;
  /** Durable stream topic that progress and tiles are published to. */
  topic: string;
  /** Point the camera at the shape's bounding box (when the shape is new). */
  frame?: boolean;
  /** The script's `validate-shape` result, when the caller already has it. */
  checked?: Validation;
};

/** What `validate-shape` reports about a script that parses. */
export type Validation = {
  /** The shape's extent above the floor; null when nothing is there. */
  bounds: { min: [number, number, number]; max: [number, number, number] } | null;
  /** The camera the script sets, if any. */
  camera?: Camera | null;
};

/** Models and effort levels the user can pick for script generation. */
// OpenAI's GPT-5.6 tiers: Sol (flagship), Terra (balanced), Luna (fastest, cheapest). parseJob
// rejects anything else, and ScriptWriter checks again.
export const MODELS = ['gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna'] as const;
/** Reasoning effort. */
export const EFFORTS = ['low', 'medium', 'high', 'xhigh'] as const;
export type Model = (typeof MODELS)[number];
export type Effort = (typeof EFFORTS)[number];
export const DEFAULT_MODEL: Model = 'gpt-5.6-sol';
export const DEFAULT_EFFORT: Effort = 'medium';

export type DescribeInput = RenderSettings & {
  description: string;
  /** Refining: the current script's id (it may have been edited by hand) and the chat's earlier requests. */
  scriptId?: string;
  history?: string[];
  /** Reference images for this turn, by id in the image store (scripts.ts). */
  imageIds?: string[];
  /** Point the camera at the shape (a new chat); refinements keep the viewer's camera. */
  frame?: boolean;
  topic: string;
  /** Script-generation model and effort; default DEFAULT_MODEL at DEFAULT_EFFORT. */
  model?: Model;
  effort?: Effort;
};

/**
 * Messages on a render's durable stream topic. The browser renders them as they arrive; because
 * the stream is durable, a reconnecting reader replays it from the start (or from a cursor).
 */
export type StreamMessage =
  | { type: 'status'; message: string }
  /** `restart`: discard what this attempt streamed so far (the model wrote text, then searched). */
  | { type: 'script-delta'; attempt: number; text: string; restart?: boolean }
  | { type: 'script'; attempt: number; script: string }
  | { type: 'camera'; camera: Camera }
  | { type: 'render'; tiles: number; tileSize: number }
  | { type: 'tile'; x: number; y: number; w: number; h: number; rgb: string }
  /** A render task's tiles, published by the task itself (crates/endpoint). */
  | { type: 'tiles'; tiles: { x: number; y: number; w: number; h: number; rgb: string }[] }
  | { type: 'done' }
  | { type: 'error'; message: string };

/**
 * A readable message for a run failure. Hatchet reports task errors as (arrays of) JSON
 * strings like `["{\"message\":\"...\",\"stack\":\"...\"}"]`, nested once per level.
 */
export function describe(err: unknown, depth = 0): string {
  if (depth > 5) return String(err);
  if (err instanceof Error) return describe(err.message, depth + 1);
  if (Array.isArray(err)) return err.length ? describe(err[0], depth + 1) : 'unknown error';
  if (err && typeof err === 'object') {
    const { message } = err as { message?: unknown };
    return message !== undefined ? describe(message, depth + 1) : JSON.stringify(err);
  }
  if (typeof err === 'string') {
    try {
      const parsed: unknown = JSON.parse(err);
      if (parsed && typeof parsed === 'object') return describe(parsed, depth + 1);
    } catch {
      // not JSON: already a plain message
    }
    // Messages built from an inner failure embed it; unwrap that part too.
    const m = err.match(/^(.*?: )(\[.*\])$/s);
    return m ? m[1] + describe(m[2], depth + 1) : explain(err);
  }
  return String(err);
}

/**
 * Hatchet's delivery failures, in words a viewer can act on. The serverless operator reports a
 * task whose connection ended early in transport terms; the common causes here are Cloudflare
 * shedding a long-running Worker request when several renders overlap, and a tile task running
 * past its endpoint's request timeout.
 */
const DELIVERY_ERRORS: [RegExp, string][] = [
  [
    /endpoint closed the websocket \(code \d+\) without a done frame|websocket read (?:failed|timed out)[^]*/,
    'the render was interrupted: Cloudflare stopped the Worker running it, which happens most when several renders run at once. Try again',
  ],
  [
    /(?:request|durable invocation) timed out(?: after [\dhms.]+| without a done frame)/,
    'the render timed out: its tiles took longer than the server allows. Try fewer samples or a smaller size, or try again when fewer renders are running',
  ],
];

function explain(message: string): string {
  for (const [pattern, text] of DELIVERY_ERRORS) {
    if (pattern.test(message)) {
      const out = message.replace(pattern, text);
      return out.charAt(0).toUpperCase() + out.slice(1);
    }
  }
  return message;
}
