//! The Hatchet serverless endpoint, independent of where it runs.
//!
//! Serves two tasks:
//! - `render-tiles`: given render settings, a camera, a shape script and a list of tiles
//!   (rectangles of pixels), build the scene once and path-trace the tiles, then publish them
//!   all as one `tiles` message to the render's durable stream topic (or, for an input without a
//!   `topic`, return them as the task's output). A render is a few of these tasks (about one per core), each with
//!   tiles spread over the image, so Hatchet hands out a few tasks per render, not one per tile.
//! - `validate-shape`: parse a shape script, so a bad script fails once, before fan-out.
//!
//! Scripts can be megabytes, too big to copy into every task's input, so tasks carry a script's
//! id (the SHA-256 of its text, in hex) and the adapter supplies a [`Scripts`] store to load it
//! from: the durable stream topic the app published it to (see [`scripts`]). The same store
//! publishes rendered tiles. Adapters (the
//! native server in `crates/server`, the Cloudflare Worker in `crates/worker`) only move bytes
//! in and out of [`handle`].

pub mod hatchet;
pub mod scripts;

use base64::Engine;
use hatchet::{error_body, verify, ActionPayload, Rejection};
use scene::{RenderParams, Tile};
use serde::Deserialize;
use serde_json::{json, Value};

pub const HEALTHCHECK_PATH: &str = "/hatchet/healthcheck";
pub const TRIGGER_PATH: &str = "/hatchet/trigger";
const RENDER: &str = "render-tiles";
const RENDER_ACTION: &str = "render-tiles:render-tiles";
/// Tiles one task may carry (a whole 7680x4320 image of 64px tiles is 8160).
const MAX_TILES_PER_TASK: usize = 8192;
/// The engine's limit on an inline stream message (4 MiB), less room for the JSON around tiles.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024 - 64 * 1024;
const VALIDATE: &str = "validate-shape";
const VALIDATE_ACTION: &str = "validate-shape:validate-shape";

pub struct Config<'a> {
    pub signing_secret: &'a str,
    pub endpoint_id: Option<&'a str>,
    /// Shown on the endpoint in the Hatchet dashboard.
    pub runtime_name: &'a str,
    /// Optional cap on render tasks running at once across all renders, declared to Hatchet as
    /// workflow concurrency so the excess waits in Hatchet's queue. A task renders on one core,
    /// so about the renderer's total cores. `None` = no overall cap.
    pub max_concurrent_tiles: Option<u32>,
    /// Optional cap on one render's tasks running at once, so a render can't take every slot
    /// from the others. `None` = no per-render cap.
    pub max_tiles_per_render: Option<u32>,
    /// Whether this endpoint renders tiles. When another endpoint (e.g. the native server)
    /// renders them, this one declares only `validate-shape`, so Hatchet sends it no tiles.
    pub serve_render: bool,
}

/// `serve_render` from the `SERVE_TASKS` setting: a comma-separated list of the tasks to serve
/// (`render-tiles`, `validate-shape`); unset or empty serves both.
pub fn serves_render(serve_tasks: Option<&str>) -> bool {
    match serve_tasks.map(str::trim).filter(|s| !s.is_empty()) {
        None => true,
        Some(list) => list.split(',').any(|t| t.trim() == RENDER),
    }
}

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

fn error(status: u16, message: impl Into<String>, retry: bool) -> Reply {
    Reply { status, body: error_body(message, retry) }
}

/// The engine's durable streams, as the tasks use them: scripts loaded by id, and rendered
/// tiles published to a render's topic.
// The futures needn't be `Send`: adapters await them on the request's own task.
#[allow(async_fn_in_trait)]
pub trait Scripts {
    /// The script's text; `Err` if it can't be loaded (retried, so it may be temporary).
    async fn get(&self, id: &str) -> Result<String, String>;
    /// Publishes `payload` (the body from [`scripts::publish_body`]) to the engine; `Err` if it
    /// failed (retried, so it may be temporary).
    async fn publish(&self, body: Vec<u8>) -> Result<(), String>;
}

/// A script id: the SHA-256 of the script's text, as 64 lowercase hex digits.
pub fn is_script_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidateInput {
    script_id: String,
}

#[derive(Deserialize)]
struct TilesInput {
    #[serde(flatten)]
    params: RenderParams,
    tiles: Vec<Tile>,
    #[serde(rename = "scriptId")]
    script_id: String,
    /// The render's stream topic: when set, the tiles are published there instead of returned.
    #[serde(default)]
    topic: Option<String>,
}

/// Loads a task's script, or the reply that fails the task.
async fn load(scripts: &impl Scripts, id: &str) -> Result<String, Reply> {
    if !is_script_id(id) {
        return Err(error(400, "scriptId must be a script id (64 hex digits)", false));
    }
    scripts.get(id).await.map_err(|e| error(503, format!("loading script {id}: {e}"), true))
}

/// Handle a POST to one of the two routes.
pub async fn handle(
    path: &str,
    signature: &str,
    body: &[u8],
    cfg: &Config<'_>,
    now_unix_secs: i64,
    scripts: &impl Scripts,
) -> Reply {
    let envelope = match verify(body, signature, cfg.signing_secret, now_unix_secs, cfg.endpoint_id) {
        Ok(e) => e,
        Err(Rejection::BadSignature) => return error(401, "invalid signature", false),
        Err(Rejection::Stale) => return error(401, "stale timestamp", false),
        Err(Rejection::WrongEndpoint) => return error(403, "wrong endpoint", false),
        Err(Rejection::Malformed(e)) => return error(400, format!("malformed body: {e}"), false),
    };

    if path == HEALTHCHECK_PATH {
        return Reply { status: 200, body: healthcheck(cfg) };
    }

    let Some(action) = envelope.action else {
        return error(400, "trigger body has no action", false);
    };
    match action.action_id.as_str() {
        RENDER_ACTION if cfg.serve_render => render(&action.action_payload, scripts).await,
        VALIDATE_ACTION => validate(&action.action_payload, scripts).await,
        _ => error(404, "unknown action", false),
    }
}

async fn validate(payload: &str, scripts: &impl Scripts) -> Reply {
    let input = match serde_json::from_str::<ActionPayload<ValidateInput>>(payload) {
        Ok(p) => p.input,
        Err(e) => return error(400, format!("invalid input: {e}"), false),
    };
    let script = match load(scripts, &input.script_id).await {
        Ok(s) => s,
        Err(reply) => return reply,
    };
    match scene::validate_scene(&script) {
        Ok(v) => Reply { status: 200, body: json!({ "valid": true, "bounds": v.bounds, "camera": v.camera }) },
        Err(e) => error(400, e, false),
    }
}

async fn render(payload: &str, scripts: &impl Scripts) -> Reply {
    let TilesInput { mut params, tiles, script_id, topic } = match serde_json::from_str::<ActionPayload<TilesInput>>(payload) {
        Ok(p) => p.input,
        Err(e) => return error(400, format!("invalid input: {e}"), false),
    };
    if tiles.is_empty() || tiles.len() > MAX_TILES_PER_TASK {
        return error(400, format!("a task carries 1 to {MAX_TILES_PER_TASK} tiles"), false);
    }
    if let Some(e) = tiles.iter().find_map(|t| params.validate(t).err()) {
        return error(400, e, false);
    }
    params.shape = match load(scripts, &script_id).await {
        Ok(s) => s,
        Err(reply) => return reply,
    };

    let mut out = Vec::with_capacity(tiles.len());
    let rendered = scene::render_tiles(&params, &tiles, |tile, rgb| {
        out.push(json!({
            "x": tile.x, "y": tile.y, "w": tile.w, "h": tile.h,
            // Packed 8-bit sRGB, row-major, w * h * 3 bytes.
            "rgb": base64::engine::general_purpose::STANDARD.encode(rgb),
        }));
        Ok(())
    });
    if let Err(e) = rendered {
        return error(400, e, false); // bad script: retrying won't help
    }
    let Some(topic) = topic else {
        return Reply { status: 200, body: json!({ "tiles": out }) };
    };

    // One message for the task's tiles. The producer id is the task's first tile, which no other
    // task of the render has, so a retry of this task publishes nothing new.
    let count = out.len();
    let message = json!({ "type": "tiles", "tiles": out }).to_string();
    if message.len() > MAX_MESSAGE_BYTES {
        return error(400, format!("a task's tiles came to {} bytes, over the stream's {MAX_MESSAGE_BYTES}", message.len()), false);
    }
    let producer = format!("tiles-{}-{}", tiles[0].x, tiles[0].y);
    if let Err(e) = scripts.publish(scripts::publish_body(&topic, &producer, message.as_bytes())).await {
        return error(503, format!("publishing tiles to {topic}: {e}"), true);
    }
    Reply { status: 200, body: json!({ "tiles": count }) }
}

/// The render tasks' concurrency chain; a task runs once it passes every entry, in order.
/// 1. Per render (round-robin over renders): one render's tasks in flight, keyed by the
///    `render_id` the app tags every run with (metadata propagates to children). Tasks of
///    several renders then share the slots instead of the oldest render taking all of them.
/// 2. Overall: every task of every render, so the renderer's machines aren't oversubscribed.
fn tile_concurrency(cfg: &Config) -> Option<Value> {
    let rr = "GROUP_ROUND_ROBIN";
    let mut chain = vec![];
    if let Some(max_runs) = cfg.max_tiles_per_render {
        let key = "has(additional_metadata.render_id) ? additional_metadata.render_id : 'none'";
        chain.push(json!({ "expression": key, "maxRuns": max_runs, "limitStrategy": rr }));
    }
    if let Some(max_runs) = cfg.max_concurrent_tiles {
        chain.push(json!({ "expression": "'tiles'", "maxRuns": max_runs, "limitStrategy": rr }));
    }
    (!chain.is_empty()).then(|| Value::Array(chain))
}

fn healthcheck(cfg: &Config) -> Value {
    let mut render = json!({
        "name": RENDER,
        "description": "Path-trace tiles of a user-scripted scene",
        // 5xx responses (including Cloudflare's own, e.g. CPU-limit kills under load) are
        // retried with exponential backoff: 2s, 4s, 8s, 16s, then every 30s, so retries
        // don't land in the same burst that caused the failure. 4xx are not retried.
        // When renders overlap, their tasks wait for slots behind earlier renders' (several
        // minutes for a big render): Hatchet's default schedule timeout (5m) would fail them.
        "tasks": [{
            "readableId": RENDER, "action": RENDER_ACTION, "timeout": "300s", "scheduleTimeout": "30m",
            "retries": 8, "backoffFactor": 2.0, "backoffMaxSeconds": 30,
        }],
    });
    if let Some(chain) = tile_concurrency(cfg) {
        render["concurrencyArr"] = chain;
    }
    let validate = json!({
        "name": VALIDATE,
        "description": "Check that a shape script parses",
        "tasks": [{ "readableId": VALIDATE, "action": VALIDATE_ACTION, "timeout": "30s", "retries": 2 }],
    });
    let workflows = if cfg.serve_render { vec![render, validate] } else { vec![validate] };
    json!({
        "workflows": workflows,
        "durable": { "supported": false },
        "runtime": { "name": cfg.runtime_name, "sdkVersion": env!("CARGO_PKG_VERSION") },
    })
}
