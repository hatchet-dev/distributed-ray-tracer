// Types shared by the workflows (workflows.ts) and the render API (index.ts).

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
// Not Fable: too costly for this app. parseJob rejects anything else, and ScriptWriter checks again.
export const MODELS = ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-haiku-5-5'] as const;
export const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;
export type Model = (typeof MODELS)[number];
export type Effort = (typeof EFFORTS)[number];

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
  /** Script-generation model and effort; default claude-opus-5-5 at medium. */
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
    return m ? m[1] + describe(m[2], depth + 1) : err;
  }
  return String(err);
}
