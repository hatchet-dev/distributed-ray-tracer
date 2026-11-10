// Validation of a render request (POST /api/render).
import { DEFAULT_EFFORT, DEFAULT_MODEL, EFFORTS, MODELS, type Effort, type Model, type RenderSettings } from './messages.js';
import { MAX_IMAGES } from './scripts.js';

/** Bounds checked up front so a bad request doesn't spawn thousands of failing tiles. */
const LIMITS = { width: [1, 7680], height: [1, 4320], samples: [1, 4096], bounces: [0, 16] } as const;
/** Matches crates/scene; fits in one durable stream message, where scripts are stored (scripts.ts). */
export const MAX_SCRIPT_BYTES = 3 * 1024 * 1024;
const MAX_DESCRIPTION_CHARS = 2000;
const MAX_HISTORY = 30;

/** A render to start: either a script to render or a description to turn into one. */
export type Job = RenderSettings &
  (
    | { shape: string; frame: boolean }
    | { description: string; model: Model; effort: Effort; script?: string; history: string[]; imageIds: string[]; frame: boolean }
  );

/** The job described by a request body, or an error message. */
export function parseJob(body: Record<string, unknown>): Job | string {
  const nums: Record<string, number> = {};
  for (const [key, [min, max]] of Object.entries(LIMITS)) {
    const v = Number(body[key]);
    if (!Number.isInteger(v) || v < min || v > max) return `${key} must be an integer in ${min}..${max}`;
    nums[key] = v;
  }
  const { camera = {}, shape, description } = body;
  if (typeof camera !== 'object' || camera === null || Array.isArray(camera)) return 'camera must be an object';
  const settings = {
    ...(nums as Pick<RenderSettings, 'width' | 'height' | 'samples' | 'bounces'>),
    camera: camera as RenderSettings['camera'],
  };

  if (typeof description === 'string' && description.trim()) {
    if (description.length > MAX_DESCRIPTION_CHARS) return `description must be under ${MAX_DESCRIPTION_CHARS} characters`;
    const { model = DEFAULT_MODEL, effort = DEFAULT_EFFORT } = body;
    if (!MODELS.includes(model as Model)) return `model must be one of ${MODELS.join(', ')}`;
    if (!EFFORTS.includes(effort as Effort)) return `effort must be one of ${EFFORTS.join(', ')}`;
    const { script, history = [], imageIds = [] } = body;
    if (!Array.isArray(imageIds) || imageIds.length > MAX_IMAGES || !imageIds.every((id) => typeof id === 'string' && /^[0-9a-f]{64}$/.test(id))) {
      return `imageIds must be at most ${MAX_IMAGES} ids from POST /api/images`;
    }
    if (script !== undefined && (typeof script !== 'string' || !script.trim())) return 'script must be a non-empty string';
    if (script !== undefined && tooBig(script)) return `script must be under ${MAX_SCRIPT_BYTES} bytes`;
    if (
      !Array.isArray(history) ||
      history.length > MAX_HISTORY ||
      !history.every((h) => typeof h === 'string' && h.length <= MAX_DESCRIPTION_CHARS)
    ) {
      return `history must be at most ${MAX_HISTORY} earlier requests`;
    }
    return {
      ...settings,
      description: description.trim(),
      model: model as Model,
      effort: effort as Effort,
      ...(script !== undefined ? { script } : {}),
      history: history as string[],
      imageIds: imageIds as string[],
      frame: script === undefined || body.frame === true,
    };
  }
  if (typeof shape !== 'string' || !shape.trim()) return 'provide a shape script or a description';
  if (tooBig(shape)) return `shape script must be under ${MAX_SCRIPT_BYTES} bytes`;
  return { ...settings, shape, frame: body.frame === true };
}

const tooBig = (script: string) => new TextEncoder().encode(script).length > MAX_SCRIPT_BYTES;

/** The workflow a job starts. */
export const workflowFor = (job: Job) => ('description' in job ? 'describe-and-render' : 'render-scene');
