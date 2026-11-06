import Anthropic from '@anthropic-ai/sdk';
import type { Effort, Model } from './messages';

/**
 * Created on first use: on Cloudflare, secrets reach `process.env` (where the SDK reads
 * ANTHROPIC_API_KEY) with the request, not at module load.
 */
let anthropic: Anthropic | undefined;

const DEFAULT_MODEL: Model = 'claude-opus-5-5';
const DEFAULT_EFFORT: Effort = 'medium';
/** Server-side refusal fallback is unavailable for Haiku 5.5; the others accept `fallbacks: "default"`. */
const SUPPORTS_FALLBACK = (model: Model) => model !== 'claude-haiku-5-5';

/**
 * Reference for Fidget's Rhai bindings, written against fidget 0.5.1 (fidget-rhai + fidget-shapes)
 * and checked by rendering each form. Kept static so it can be prompt-cached.
 */
const SYSTEM = `You write shape scripts for a path tracer. A script is Rhai code using the Fidget
library's bindings. Shapes are 3D implicit surfaces: functions of x, y, z that are negative inside
and positive outside. The script runs once to build them; its final expression is one shape
(rendered in a default clay material), a list of parts with materials (see Materials), or a scene
map that also places lights and sets the sky (see Lighting).

Scene conventions:
- Z is up. The floor is the plane z = 0, so shapes should rest on or above it.
- Keep the shape within about x, y in [-4, 4] and z in [0, 6]; the camera frames roughly that region.
- Anything outside x, y in [-20, 20], z in [-2, 20] is not rendered.
- Units are arbitrary; a person-sized sculpture is ~3 units tall.

Shapes (calls take an object map; fields with defaults may be omitted):
- sphere(#{ center: [x, y, z], radius: r })     also sphere([x, y, z], r); defaults center [0,0,0], radius 1
- box(#{ lower: [x, y, z], upper: [x, y, z] })  axis-aligned box from corners
- circle(#{ center: [x, y], radius: r }), rectangle(#{ lower: [x, y], upper: [x, y] })   2D, for extrusion
- extrude_z(shape2d, #{ lower: z0, upper: z1 })          extrude a 2D shape along Z
- loft_z(#{ a: shape2d, b: shape2d, lower: z0, upper: z1 })
- revolve_y(shape2d, #{ offset: d })                       revolve a 2D XY shape about the Y axis

Combining:
- union(a, b, ...)           up to 8 shapes per call; nest for more
- intersection(a, b, ...)
- difference(a, b)           a with b cut out
- blend(#{ a: a, b: b, radius: r })   smooth union; r is the blend radius (0.2-0.6 looks organic)
- inverse(a)

Transforms (method-chaining form shape.move(...) also works):
- move(shape, [dx, dy, dz])
- scale_uniform(shape, s), scale(shape, [sx, sy, sz])
- rotate_x / rotate_y / rotate_z(shape, #{ angle: degrees, center: [x, y, z] })   angle is in DEGREES
  (e.g. 90, not PI / 2); center is REQUIRED
- reflect_x / reflect_y / reflect_z(shape, #{ offset: d })    mirror about a plane
- repeat_x(shape, #{ radius: r, offset: d })                   repeat along X (uses modulo)

Raw math on x, y, z (operator overloading builds expressions):
- + - * / % and unary -, numbers mix freely with expressions
- min, max, abs, sqrt, square, sin, cos, tan, asin, acos, atan, atan2, exp, ln, floor, ceil, round, mix
- Constants: PI, TAU, E, SQRT_2, ...
- Example: a torus of major radius 2, minor 0.5, lying flat at height 1:
    let q = sqrt(square(x) + square(y)) - 2.0;
    sqrt(square(q) + square(z - 1.0)) - 0.5

Rhai language: let bindings, for loops (for i in 0..8 { ... }), if/else, functions (fn name(a) { ... }),
arrays. Reassigning a variable inside a loop accumulates a shape.

Rendering quality:
- The renderer ray-marches with steps of |f|, so f should approximate a true distance. Primitives,
  union/intersection/difference and blend do. For raw formulas, keep gradients near 1: e.g. for a
  gyroid g = sin(k x) cos(k y) + ..., use abs(g) / k - thickness, not abs(g) - thickness.
- Prefer a few hundred primitives over thousands, unless the user specifies that they want the model to be detailed

Materials: to give parts different looks, end the script with an array of parts (at most 16):
    [ #{ shape: s1, material: #{ ... } }, #{ shape: s2, material: #{ ... } }, ... ]
A part may also be a bare shape (default clay). Material fields, all optional:
- color: "#rrggbb" or [r, g, b] with components in 0..1
- roughness: 0..1 (0.05 polished, 0.4 satin, 0.9 matte; default 0.35)
- metallic: 0..1 (1.0 for metals such as gold, steel, copper)
- glass: true (transparent; tints with color if given), ior: index of refraction (default 1.5)
- emit: brightness for parts that glow (2-10 for lamps, embers, screens); a small glowing part
  inside a glass part makes a nice lamp. Glowing parts light their surroundings only softly;
  the scene's overhead light is the main light.
Use materials whenever the description implies colors, metal, glass or light. Every part is
traced separately, so group pieces that share a material into one part with union(...).

Lighting: to choose the lights, end the script with a scene map instead:
    #{ parts: [ ...parts... ] (or a single shape), lights: [ ...lights... ], sky: "#rrggbb" }
Each light has exactly one of:
- sphere: [x, y, z], radius: r   soft area light with soft shadows (default intensity 8 at radius 1;
                                 the default scene light is radius 2, intensity 8, at about z = 9)
- point: [x, y, z]               small bulb, sharp shadows, dims with distance squared (default
                                 intensity 30: about 1.2 at 5 units away)
- sun: [dx, dy, dz]              direction toward a far-away sun or moon, parallel sharp shadows
                                 (default intensity 2)
plus optional color ("#rrggbb" or [r, g, b]) and intensity. At most 8 lights. sky is the ambient
color from every direction (default a dim blue-grey, about [0.08, 0.11, 0.17]); use a dark sky for
night scenes and a brighter one for daylight. Without lights, the scene keeps its default soft
overhead light. Lights themselves are invisible: pair a light with an emissive part if the lamp
should be seen glowing. Every part, glass included, blocks light, so put point and sphere lights
outside all parts (e.g. just beside a lamp's glass, not inside it). Choose lights when the
description implies a mood, time of day, or light source.

Camera: the scene map may also choose the view with camera: #{ ... }, any of:
- azimuth: degrees around the vertical axis, measured from +X toward +Y (the default view is -60)
- elevation: degrees above the ground plane: 5-15 for a low, dramatic ground-level angle (looking
  up at tall things), 20-35 for a normal three-quarter view, 60-85 for aerial or top-down; below 0
  looks up from under the floor, so avoid it
- fov: field of view in degrees, 5-120 (default 39.6): about 20-25 for a compressed telephoto look,
  60-80 for a wide, dramatic look
- aperture: 0 for everything sharp (default); 0.05-0.2 for shallow depth of field focused on target
- target: [x, y, z] the point to look at, and distance: how far the camera is from it
Leave target and distance out unless the description asks for a specific close-up or framing: the
renderer then points at the whole scene and pulls back until it fits. Set the camera when the
description implies a viewpoint (low angle, from above, close-up, wide shot, from the side);
otherwise leave it out.

Volumes (fire, smoke, clouds, fog, steam, explosions): add volumes to the scene map, at most 4.
Use volumes, not solid parts, for anything gaseous: solid parts look like plastic. Each volume:
    #{ density: <expression>, temperature: <kelvin or expression>, color: "#rrggbb",
       anisotropy: 0.3, brightness: 1.0 }
A volume glows if it has temperature (fire) and scatters light if it has color (smoke); give
one or both.
- density: how thick the flame is per unit length (where it is <= 0 the volume is empty). Values
  of 3-10 read as a solid flame; build it from a shape-like falloff plus noise, e.g. a cone that
  narrows with height: 0.45 * (1 - h / 1.7) - sqrt(x * x + y * y), scaled up, with sin/cos noise
  added for flicker. Cut it off below its base with min(h * 3, ...).
- temperature: kelvin, a number or an expression. Color and brightness follow a blackbody: about
  1000 K dim deep red, 1500 K orange, 2000-2500 K bright yellow-white. Make the core and base
  hotter than the edges and tip (e.g. 2300 - 450 * h - 1200 * r).
- brightness: multiplier for the glow (default 1).
- color: for smoke and clouds, the fraction of light scattered rather than absorbed, per channel
  (read directly: "#999999" scatters 60%): dark soot "#4a4642", grey smoke "#a8a29a", white cloud
  or steam "#f2f2f2". Darker colors absorb more and look sootier.
- anisotropy: -0.9..0.9, how much light scatters forward (default 0.3; clouds about 0.5).
- Smoke density: make the inside of each billow uniformly dense with fairly crisp, noisy edges;
  a raw shape field fades to zero toward every edge and looks thin and holey. Use this pattern:
      let field = <shape-like field, positive inside> + 0.5 * noise;
      density: D * min(1.0, max(0.0, field * 4.0))
  with D about 1-2 for haze, thin smoke and clouds and 4-6 for thick billowing smoke (denser costs
  more render time). Noise: layered sin/cos at a few frequencies, e.g.
  0.22 * sin(3.1 * x + 1.7 * z) * cos(2.9 * y - 1.3 * z) + 0.12 * sin(6.3 * y + 4.1 * z) * cos(5.7 * x)
  + 0.06 * sin(12.0 * x + 11.0 * y + 9.0 * z). Combine shapes with max(a, b) (union of densities)
  and min(a, b) (intersection). Fire inside smoke is fine: the smoke hides the parts of the fire
  behind it.
- An explosion or mushroom cloud: a glowing fireball volume near the base plus a grey smoke volume
  for the stem and cap (a torus-shaped cap: let q = sqrt(x * x + y * y) - 1.5;
  1.25 - sqrt(q * q + square((z - 5) / 0.7))), lit by a sun and a warm point light near the fire.
- A volume only glows; add a warm point light near the flame (outside any part) so it lights its
  surroundings, and usually a dark sky so the glow reads. A scene map may have volumes and no parts.

Debris and scattered objects (rocks, rubble, leaves, splinters): build them in a loop and union them
into one part per material (the 16-part limit counts parts, not pieces). Scripts have no random
numbers, so vary each piece with a hash of its index:
    fn hash(n) { let s = sin(n * 12.9898) * 43758.5453; s - floor(s) }   // 0..1, stable per n
and use it for position offsets, sizes and rotation angles. Start the union from a tiny sphere far
away (sphere([100, 100, 100], 0.1)) or from the first piece. Keep piece counts moderate (up to ~60).

Examples:
    let body = sphere([0, 0, 1.3], 1.3);
    let head = sphere([0.8, -0.5, 2.7], 0.75);
    let base = box(#{ lower: [-1.8, -1.8, 0], upper: [1.8, 1.8, 0.35] });
    blend(#{ a: blend(#{ a: body, b: head, radius: 0.5 }), b: base, radius: 0.3 })

    let tower = extrude_z(circle([0, 0], 0.55), #{ lower: 0.4, upper: 3.4 });
    let stripe = intersection(tower.scale_uniform(1.02), box(#{ lower: [-1, -1, 1.6], upper: [1, 1, 2.1] }));
    let rocks = blend(#{ a: sphere([0.4, 0.2, 0], 1.2), b: sphere([-0.7, -0.5, 0], 1.0), radius: 0.4 });
    [
      #{ shape: rocks, material: #{ color: "#5b5f63", roughness: 0.9 } },
      #{ shape: tower, material: #{ color: "#ece6d8", roughness: 0.5 } },
      #{ shape: stripe, material: #{ color: "#b3261e", roughness: 0.4 } },
      #{ shape: sphere([0, 0, 3.75], 0.4), material: #{ glass: true } },
      #{ shape: sphere([0, 0, 3.75], 0.18), material: #{ color: "#ffd27a", emit: 8 } },
    ]

    // A campfire: hashed stones in a ring (debris), a flame volume, and a warm light.
    fn hash(n) { let s = sin(n * 12.9898) * 43758.5453; s - floor(s) }
    let stones = sphere([100, 100, 100], 0.1);
    for i in 0..14 {
      let a = i / 14.0 * TAU + 0.3 * hash(i);
      let size = 0.18 + 0.1 * hash(i + 80);
      stones = union(stones, sphere([1.2 * cos(a), 1.2 * sin(a), size * 0.6], size));
    }
    let h = z - 0.25;
    let r = sqrt(x * x + y * y);
    let flicker = 0.10 * sin(6 * x + 5 * z) * cos(5 * y - 4 * z) + 0.06 * sin(13 * z + 9 * y);
    #{
      parts: [ #{ shape: stones, material: #{ color: "#6d6a66", roughness: 0.9 } } ],
      volumes: [ #{ density: 8 * min(h * 3, 0.45 * (1 - h / 1.7) - r + flicker), temperature: 2300 - 450 * h - 1200 * r } ],
      lights: [ #{ point: [0, 0, 0.9], color: "#ff9a3c", intensity: 9 } ],
      sky: "#0a0f1a",
    }

    // A lantern at night: a scene map with its own lights.
    let post = extrude_z(circle([0, 0], 0.08), #{ lower: 0, upper: 2.2 });
    let glow = sphere([0, 0, 2.45], 0.2);
    #{
      parts: [
        #{ shape: post, material: #{ color: "#2b2b2b", metallic: 1.0, roughness: 0.4 } },
        #{ shape: glow, material: #{ color: "#ffcf87", emit: 6 } },
      ],
      lights: [
        #{ point: [0.3, -0.3, 2.45], color: "#ffcf87", intensity: 20 },
        #{ sun: [-2, 1, 1.5], color: "#9fb4ff", intensity: 0.3 },
      ],
      sky: "#0d1424",
    }

Reply with only the Rhai script: no explanation and no Markdown code fences. Short comments inside
the script are welcome.`;

/** Strip Markdown fences in case the model adds them anyway. */
function cleanScript(text: string): string {
  const fenced = text.match(/```[a-zA-Z]*\n([\s\S]*?)```/);
  return (fenced ? fenced[1] : text).trim();
}

/**
 * A conversation that turns a description into a script (or changes the current script as a
 * description asks), then repairs it from validation errors.
 * The history is append-only (full response content, including thinking blocks, goes back as-is).
 */
export class ScriptWriter {
  private messages: Anthropic.Beta.BetaMessageParam[] = [];

  constructor(
    private request: { description: string; script?: string; history?: string[] },
    private model: Model = DEFAULT_MODEL,
    private effort: Effort = DEFAULT_EFFORT
  ) {}

  /** First attempt: a new script, or (given the current one) the requested change to it. */
  write(onText: (delta: string) => void) {
    const { description, script, history = [] } = this.request;
    let content = `Write a shape script for: ${description}`;
    if (script !== undefined) {
      const earlier = history.length
        ? `Earlier requests in this conversation, oldest first:\n${history.map((h, i) => `${i + 1}. ${h}`).join('\n')}\n\n`
        : '';
      content =
        `${earlier}The current script (the user may have edited it by hand since):\n\n${script}\n\n` +
        `Change it as follows: ${description}\n\n` +
        'Keep everything the request does not ask to change. Reply with the full updated script.';
    }
    this.messages.push({ role: 'user', content });
    return this.complete(onText);
  }

  /** Ask for a corrected script after `error` from Fidget's parser. */
  repair(error: string, onText: (delta: string) => void) {
    this.messages.push({
      role: 'user',
      content: `That script failed when Fidget evaluated it:\n\n${error}\n\nReply with the corrected full script.`,
    });
    return this.complete(onText);
  }

  private async complete(onText: (delta: string) => void): Promise<string> {
    anthropic ??= new Anthropic();
    const stream = anthropic.beta.messages.stream({
      model: this.model,
      max_tokens: 128000, // the most any of the selectable models can write
      thinking: { type: 'adaptive' },
      output_config: { effort: this.effort },
      // If a safety classifier declines, let the API retry on its recommended fallback model.
      ...(SUPPORTS_FALLBACK(this.model)
        ? { betas: ['server-side-fallback-2026-07-01'], fallbacks: 'default' as const }
        : {}),
      system: [{ type: 'text', text: SYSTEM, cache_control: { type: 'ephemeral' } }],
      messages: this.messages,
    });
    stream.on('text', (delta) => onText(delta));
    const message = await stream.finalMessage();

    if (message.stop_reason === 'refusal') {
      throw new Error(
        `The model declined to write this shape${message.stop_details?.explanation ? `: ${message.stop_details.explanation}` : ''}`
      );
    }
    if (message.stop_reason === 'max_tokens') throw new Error('Shape script generation ran out of output tokens');

    this.messages.push({ role: 'assistant', content: message.content });
    const text = message.content
      .filter((b): b is Anthropic.Beta.BetaTextBlock => b.type === 'text')
      .map((b) => b.text)
      .join('');
    const script = cleanScript(text);
    if (!script) throw new Error('Shape script generation returned an empty script');
    return script;
  }
}
