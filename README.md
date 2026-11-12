# Distributed Ray Tracer

This demo renders images through a ray-tracing technique called Signed Distance Fields. The entire ray-traced scene is represented as code and written by a language model. Because this technique is embarrassingly parallel, each tile of the image can render simultaneously up to a very high resolution. [Hatchet](https://hatchet.run) is used to orchestrate the parallel tiles, using [Cloudflare Workers running Rust](https://developers.cloudflare.com/workers/languages/rust/) as the underlying compute (with an option to run on even larger, standalone machines if needed). The coding agent also uses Hatchet + Cloudflare and durable streams are used for passing messages back to each frontend.

This was built to showcase two new Hatchet features:

- [Serverless endpoints](https://docs.hatchet.run/v1/serverless)
- [Durable streams](https://docs.hatchet.run/reference/typescript/feature-clients/streams)

Signed Distance Fields are a technique popularized by [Inigo Quilez](https://iquilezles.org/articles/raymarchingdf/). This renderer uses a DSL based on [Rhai](https://github.com/rhaiscript/rhai) and [Fidget](https://github.com/mkeeter/fidget) to define the SDFs, which you can edit yourself. This approach is uniquely suited to refining with LLMs because it represents the entire ray-traced scene as code.

The renderer itself is a slightly modified version of [rpt](https://github.com/ekzhang/rpt).

## How it works

You describe a shape in a chat; a language model writes it as a Fidget (Rhai) script, and a
`render-scene` workflow fans the image out as a few `render-tiles` tasks (about one per core), each
rendering tiles spread over the image. Hatchet's serverless operator delivers each task to the Rust
tile server, and finished tiles stream back to the browser through a durable stream as they complete.

```
 browser ──POST /api/render──► app Worker ── runNoWait ──► Hatchet ──► app Worker (/hatchet/*)
    ▲          (one streaming      │                                  describe-and-render: model writes
    │           response)          │                                    or changes the script, validated
    │                              │                                  render-scene: fan out tiles ─────┐
    └──── SSE ◄── streams.subscribe(render-<id>) ◄── publish each tile                                 │
                                                                                                       ▼
                                                     tile server (Rust): render-tiles, validate-shape
```

Keep chatting to refine the shape ("make it gold", "add a second tower", "view it from above"): each
message sends the current script, including any edits you made by hand, with the earlier requests,
and the model returns the full updated script. A script's last expression is the scene: one shape,
a list of parts with materials, or a map that also places lights, sky, fire and smoke volumes, and
the camera. The render options (resolution, maximum samples per pixel, bounces) live in their own panel.

**Preview.** Next to the rendered image (switch with the Preview / Render buttons or P), the page
shows a live 3D preview of the script: each part meshed in the browser by Fidget's mesher
(`crates/preview`, compiled to wasm) and drawn with three.js. It
updates as you edit the script. Drag the background to orbit the view, shift-drag to pan, scroll to
zoom (on a phone: drag with one finger, pinch to zoom); arrow keys walk the view around (shift+arrows turn and tilt it). The render camera is an object in the scene, drawn with its frustum: click it to move it (W)
or turn it (E; left/right and up/down, since the renderer's camera is always level), and the next
render is taken from there. Once placed by hand, renders keep the camera where it is instead of
re-framing it on the shape (a new chat frames afresh). "Camera view" (C) shows exactly what
the render will frame; then dragging and scrolling orbit and zoom the render camera itself.

Parts with a `label` and an `at: [x, y, z]` can be moved and turned: click one (or its label), then
drag its arrows (W) or its rings (E, about x, y and z). On release the preview rewrites that part's
`at` or `rotate: [rx, ry, rz]` (degrees, about x, then y, then z) in the script, or adds a `rotate`
after its `at`. The text is found in the script's syntax tree, not by the model, so it stays right
after hand edits. Parts whose `at` is the same variable (`let lamp_at = [...]`) move and turn
together; computed positions show, but can't be moved. The model is asked to label every part and
place it this way.

## Layout

| Path | What |
| --- | --- |
| `crates/scene` | The scene (`build_scene`), the orbit camera (`CameraParams`), and `render_tile`. Built on [rpt](https://crates.io/crates/rpt) (path tracing, materials, lights) and [Fidget](https://crates.io/crates/fidget) (parses and evaluates shape scripts). `implicit.rs` ray-marches a Fidget shape as an rpt `Shape`; `volume.rs` renders fire and smoke. |
| `crates/endpoint` | Hatchet's [serverless HTTP contract](https://docs.hatchet.run/v1/serverless/http) in Rust, independent of any runtime: signature checks, the healthcheck that declares `render-tiles` and `validate-shape`, and the trigger route. |
| `crates/worker` | The tile Worker: a Cloudflare adapter for `crates/endpoint` (`workers-rs`, compiled to wasm). |
| `crates/preview` | The page's preview, compiled to wasm for the browser: meshes each part (`mesh.rs`) and finds where each part's `at` is written (`handles.rs`). `scripts/build-preview.sh` builds it into `app/public/preview/`; `wrangler dev` and `wrangler deploy` in `app/` run it. |
| `crates/server` | The same tile endpoint as a native HTTP server (axum), for a container: `deploy/tile.Dockerfile`. |
| `app` | Everything else (TypeScript): the page (`public/`, with the preview in `preview-view.js` and `preview-worker.js`), the render API (`src/app.ts`), and the workflows `describe-and-render` and `render-scene` (`src/workflows.ts`) served under `/hatchet` with [`@hatchet-dev/serverless`](https://www.npmjs.com/package/@hatchet-dev/serverless). It runs as one Cloudflare Worker (`src/cloudflare.ts`) or one Node server (`src/node.ts`, `deploy/app.Dockerfile`); `src/env.ts` is what differs between them. `src/describe.ts` is the model call (OpenAI). |

## Deploy

Both Workers need the Workers Paid plan (for `cpu_ms = 300000`) and a Hatchet tenant with the
serverless operator and durable streams enabled.

```sh
# The tile Worker
cd crates/worker && pnpm install
openssl rand -hex 32 | tee .signing-secret | pnpm exec wrangler secret put HATCHET_SIGNING_SECRET
pnpm exec wrangler secret put HATCHET_CLIENT_TOKEN   # tenant API token: reads scripts from their topics
pnpm exec wrangler deploy

# The app Worker
cd ../../app && pnpm install
openssl rand -hex 32 > .signing-secret
tr -d '\n' < .signing-secret | pnpm exec wrangler secret put HATCHET_SIGNING_SECRET
pnpm exec wrangler secret put HATCHET_CLIENT_TOKEN   # tenant API token
pnpm exec wrangler secret put OPENAI_API_KEY
pnpm exec wrangler deploy
```

Then register each Worker's `https://<worker>.workers.dev/hatchet/healthcheck` and `/hatchet/trigger`
as a `CLOUDFLARE_WORKERS` endpoint, from the dashboard or with
`POST /api/v1/stable/tenants/{tenant}/serverless/endpoints`, using that Worker's signing secret.

The URL is public, so put it behind Cloudflare Access before sharing it.

### As containers (e.g. Azure via Porter)

Both halves also run as plain HTTP servers, with the same environment variable names as the
Workers' secrets. Each serves Hatchet's endpoint at `/hatchet/healthcheck` and `/hatchet/trigger`,
and `GET /healthz` for the platform's health probe; both listen on `PORT` (default 8080).

```sh
# The tile server (Rust, native): HATCHET_SIGNING_SECRET, HATCHET_CLIENT_TOKEN
docker build -f deploy/tile.Dockerfile -t ray-tile-server .

# The app (Node): HATCHET_SIGNING_SECRET, HATCHET_CLIENT_TOKEN, OPENAI_API_KEY
scripts/build-app.sh      # the preview module and app/dist/server.mjs (see the SDK note)
docker build -f deploy/app.Dockerfile -t ray-tracer-app .
```

Locally, `pnpm run build:node && pnpm start` in `app/` serves the app with `app/.env`.
Mixing is fine: the app on a container with tiles on the Worker, or the other way around; Hatchet
only needs each endpoint registered with its URL and signing secret.

**Local development:** `pnpm exec wrangler dev` in `app/` serves the page and the render API with the
secrets in `app/.dev.vars`. The workflows still run on the deployed Workers that Hatchet calls.

**SDK note:** the render API reads the durable stream with `streams.subscribe()` on
`@hatchet-dev/typescript-sdk/core`, which the published SDK doesn't have yet. Until it ships,
`app/package.json` links a local SDK build (`link:` to its `dist/`); build it there with
`pnpm run tsc:build && pnpm run prepublish`.

## How it works

- **Parts and materials.** A script ends with either one shape (rendered in clay, as is any part without a material) or a list of up
  to 16 parts, `[#{ shape, material: #{ color, roughness, metallic, glass, ior, emit } }, ...]`.
  Each part becomes its own marched shape with an rpt material (`crates/scene/src/parts.rs`); unknown
  or invalid fields fail validation with a clear message, which the repair loop feeds back to the
  model. Glowing (`emit`) parts are seen and light their surroundings through bounced light only; the
  overhead area light stays the main light.
- **Lights, sky and fire.** A script can end with a scene map
  `#{ parts, lights, sky, volumes }`. Lights are rpt's sphere (area), point and directional ("sun")
  lights; without any, a default overhead light is used. `volumes` are gas: fire glows (a `temperature` field in
  kelvin, mapped to blackbody color) and smoke scatters light (a `color` albedo, Henyey-Greenstein
  phase function); both take a Fidget `density` field. They are rendered with delta tracking
  (emission, absorption, scattering with direct light at each scattering point) and shadow rays use
  ratio tracking, as in pbrt-v4; a furnace test checks scattering conserves energy. Majorants come from a grid of density upper bounds proven by Fidget's interval arithmetic
  (`crates/scene/src/volume.rs`). Debris is built in-script with loops and a sine hash.
- **Scripts.** A script can be up to 3 MB and run up to 10 million Rhai steps (Fidget's default is
  50,000). Too big to copy into every task's input, each script is published once to its own
  durable stream topic, `script-<sha256>` (`app/src/scripts.ts`), and tasks carry that `scriptId`.
  The tile Worker reads the topic with the engine's `Subscribe` call over Connect (plain HTTP, so no
  gRPC client; `crates/endpoint/src/scripts.rs`), keeps the last few scripts in memory, and needs
  `HATCHET_CLIENT_TOKEN` for it. 3 MB fits in one stream message (just under 4 MiB), and the topic
  keeps it for the tenant's stream retention. The model can write at most 128K output tokens (about
  400 KB of script, less what it spends thinking), and refining a script sends it in the prompt,
  so the largest scripts have to be written or pasted by hand.
- **Background.** By default the background is black: the floor plane at z = 0 absorbs all light
  (rpt's "diffuse" material still has a Fresnel highlight, so a black material wouldn't do), and the
  default dim sky lights the scene but shows black to camera rays and rays straight through glass.
  A script opts in with the scene map's `floor` (a color, `true`, or a material map with an optional
  `shader`) and `sky`.
- **Shaders.** A part can have a `shader: fn(p, n)` that returns any of `color`, `roughness`,
  `metallic` and `emit` (overriding its `material` point by point) and `bump`, a height whose
  slope along the surface tilts the shading normal (bump mapping; its gradient comes from Fidget's
  gradient evaluator, and the tilt is capped at 70°), and the scene's `sky` can be a
  `fn(d)` returning a color. Like shapes, shaders run once with symbolic inputs (`p` is x, y, z;
  `n` is three extra Fidget variables), so each output becomes a Fidget expression that every hit
  evaluates (`crates/scene/src/shader.rs`); they're branch-free, using registered `step`,
  `smoothstep`, `select`, `clamp`, `fract` and a three-argument `mix` (Fidget's own two-argument
  `mix` is a pseudo-random mix). Wrong signatures, unknown fields or wrong types fail in
  `validate-shape` with a message the repair loop sends back to the model.
- **Reference images.** A chat message can carry up to 6 images (attach, paste, or drop them on the
  chat). The page shrinks each to what the model reads (at most 2576 px on the long edge, as JPEG,
  typically well under 1 MB) and uploads it to `POST /api/images` as soon as it is attached; the
  Worker stores it like a script, on `image-<sha256>`, and the message carries the ids.
  `describe-and-render` reads the images and puts them in the model's prompt, so they never pass
  through Hatchet task inputs. Images apply to the message they were attached to: later messages
  only mention that there were some, and the chat keeps small thumbnails in the browser.
- **Shapes.** Fidget's sandboxed Rhai engine (bounded script steps and expression depth) turns the
  script into a math expression `f(x, y, z)`: negative inside, positive outside. `implicit.rs` marches
  rays along it using `|f|` as the step, refines hits by bisection, and takes normals from Fidget's
  gradient evaluator. Fidget's JIT is off (it doesn't support wasm), so it uses its interpreter.
  Before fanning out, `render-scene` runs `validate-shape`, so a typo fails once with Rhai's error
  message instead of in every tile.
- **Requests.** The page POSTs the job (the render settings, plus a `shape` script or a chat
  message) to `/api/render`, and the response is the render's event stream, read with `fetch`. The
  Worker starts the workflow, then reads its durable stream topic and forwards each message; the
  first event, `start`, carries the render and run ids. A **Download PNG** button saves the canvas
  at full resolution.
- **Chat → script.** `describe-and-render` streams the model's script (OpenAI's Responses API, `gpt-5.6-sol` at
  medium reasoning effort by default; Terra and Luna are selectable, and it can use OpenAI's web
  search) into the editor as it is written,
  validates it with the endpoint's `validate-shape` task, and on a parse error sends the error back
  to the model to fix it, up to 3 attempts. Then it starts `render-scene` as a child workflow,
  passing the validation result along so the script isn't checked twice, and returns without
  waiting: a long-lived task can lose its connection, so only the render stays open while tiles run.
  `render-scene` publishes `done` (or `error`), and the render API watches whichever run is active.
- **Framing.** `validate-shape` also returns the shape's visible bounding box, computed with Fidget's
  interval arithmetic (it proves regions empty), in ~2 ms. When the shape is new, `render-scene`
  aims the camera at the box's center and pulls back until all 8 corners fit both fields of view,
  keeping the user's orbit angles. Renders after placing the camera in the preview don't reframe,
  and neither does a chat refinement unless it changed the camera the script sets.
- **Durable streams.** Each render publishes to one Hatchet durable stream topic (`render-<id>`):
  `status`, `script-delta`, `script`, `camera`, `render`, `tile`, then `done` or `error`. The app
  Worker relays the topic to the browser as Server-Sent Events. Durable streams are alpha and
  enabled per tenant.
- **Tiles.** rpt's `Renderer` only renders whole frames, and its per-pixel functions are private.
  `crates/scene/src/tracer.rs` ports its integrator (about 90 lines, MIT) on top of rpt's public scene,
  camera, material and light types, so any rectangle can be rendered. Tiles are seeded from their
  position, so a retried tile is identical.
- **Camera.** Each task's input carries an orbit camera: `target`, `azimuth`, `elevation`, `distance`,
  `fov`, `aperture`, with angles in degrees and a Z-up world. Missing fields use the defaults.
- **Adaptive sampling.** The samples setting is each pixel's maximum. A pixel traces at least 16
  paths (or 1/16 of the maximum, if more), then 8 at a time until its standard error, measured after
  gamma, is under 1.5 of 255 steps per channel. Flat areas stop early; edges, soft shadows, glass
  and smoke use more. On a test scene a 256 cap traces about half its paths, a 1024 cap about a
  quarter. Sampling stays deterministic per tile, so a retried tile is still identical.
- **Tile size.** Tiles shrink as (maximum) samples and bounces grow, keeping each tile's work (pixels × samples ×
  (bounces + 1)) near 10M units. That's about 25 s of CPU in the tile Worker for typical scripts;
  more complex scripts cost proportionally more.
- **Concurrency.** Every tile can be in flight at once. Cloudflare spreads them over isolates, which
  in practice gives about 10–20× parallelism from one render; tiles beyond that wait. On a fixed
  set of machines (the native tile server, `porter.yaml`), the tile endpoint declares a two-step
  concurrency chain to Hatchet: `MAX_TILES_PER_RENDER` caps one render's tiles in flight (keyed
  by its `render_id`, round-robin across renders, so a big render can't starve a small one), and
  `MAX_CONCURRENT_TILES` caps all tiles together, near the machines' total cores. The excess
  waits in Hatchet's queue. Since the native server is several times faster than the Worker,
  set `TILE_WORK` and `TILE_MAX_SIZE` on the app (see `app/wrangler.toml`) for bigger tiles.
- **Retries.** A tile that fails with a 5xx (including Cloudflare killing an overloaded isolate)
  is retried up to 8 times with exponential backoff (2 s, 4 s, 8 s, 16 s, then every 30 s); 4xx
  errors such as a bad script are not retried. A tile gives up reading its script after 20 s, so a
  stalled connection turns into a quick retry instead of a stuck render.
- **Cancellation.** Starting another render or pressing Stop cancels the run and its tiles. The
  chat's Stop button covers writing the script; once a chat turn's script is final, its render
  belongs to the Render panel, whose button becomes Stop. Cancelling stops the run and
  its tiles (`POST /api/render/<id>/cancel`). The app Worker cancels by a `render_id` metadata tag,
  which propagates to child runs. The parent also cancels any tiles it spawned after the
  cancellation arrived. Closing the tab does not cancel: see Resuming.
- **Resuming.** The page keeps the latest render's ids in `localStorage`. On load it calls
  `GET /api/render/<id>?run=<runId>`, which replays the render's durable stream from the start
  (repainting a finished image, or catching up and following one still going), and a dropped
  connection resumes the same way. Renders are kept for the tenant's stream retention.
- **Protojson.** Hatchet encodes request bodies with protojson, so `int64` fields such as
  `timestampUnixSeconds` arrive as JSON strings. The endpoint accepts both forms.
