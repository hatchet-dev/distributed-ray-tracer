//! The Hatchet serverless endpoint, independent of where it runs.
//!
//! Serves two tasks:
//! - `render-tile`: given render settings, a camera, a shape script and a rectangle of pixels,
//!   rebuild the scene and path-trace that tile.
//! - `validate-shape`: parse a shape script, so a bad script fails once, before fan-out.
//!
//! Scripts can be megabytes, too big to copy into every task's input, so tasks carry a script's
//! id (the SHA-256 of its text, in hex) and the adapter supplies a [`Scripts`] store to load it
//! from: the durable stream topic the app published it to (see [`scripts`]). Adapters (the
//! Cloudflare Worker in `crates/worker`) only move bytes in and out of [`handle`].

pub mod hatchet;
pub mod scripts;

use base64::Engine;
use hatchet::{error_body, verify, ActionPayload, Rejection};
use scene::{RenderParams, Tile};
use serde::Deserialize;
use serde_json::{json, Value};

pub const HEALTHCHECK_PATH: &str = "/hatchet/healthcheck";
pub const TRIGGER_PATH: &str = "/hatchet/trigger";
const RENDER: &str = "render-tile";
const RENDER_ACTION: &str = "render-tile:render-tile";
const VALIDATE: &str = "validate-shape";
const VALIDATE_ACTION: &str = "validate-shape:validate-shape";

pub struct Config<'a> {
    pub signing_secret: &'a str,
    pub endpoint_id: Option<&'a str>,
    /// Shown on the endpoint in the Hatchet dashboard.
    pub runtime_name: &'a str,
    /// Optional cap on tiles rendering at once, declared to Hatchet as workflow concurrency
    /// so the excess waits in Hatchet's queue. `None` = every tile at once.
    pub max_concurrent_tiles: Option<u32>,
}

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

fn error(status: u16, message: impl Into<String>, retry: bool) -> Reply {
    Reply { status, body: error_body(message, retry) }
}

/// Where task inputs' scripts are loaded from, by id.
// The futures needn't be `Send`: adapters await them on the request's own task.
#[allow(async_fn_in_trait)]
pub trait Scripts {
    /// The script's text; `Err` if it can't be loaded (retried, so it may be temporary).
    async fn get(&self, id: &str) -> Result<String, String>;
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
struct TileInput {
    #[serde(flatten)]
    params: RenderParams,
    #[serde(flatten)]
    tile: Tile,
    #[serde(rename = "scriptId")]
    script_id: String,
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
        RENDER_ACTION => render(&action.action_payload, scripts).await,
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
    let TileInput { mut params, tile, script_id } = match serde_json::from_str::<ActionPayload<TileInput>>(payload) {
        Ok(p) => p.input,
        Err(e) => return error(400, format!("invalid input: {e}"), false),
    };
    params.shape = match load(scripts, &script_id).await {
        Ok(s) => s,
        Err(reply) => return reply,
    };
    if let Err(e) = params.validate(&tile) {
        return error(400, e, false);
    }

    let rgb = match scene::render_tile(&params, &tile) {
        Ok(rgb) => rgb,
        Err(e) => return error(400, e, false), // bad script: retrying won't help
    };
    Reply {
        status: 200,
        body: json!({
            "x": tile.x, "y": tile.y, "w": tile.w, "h": tile.h,
            // Packed 8-bit sRGB, row-major, w * h * 3 bytes.
            "rgb": base64::engine::general_purpose::STANDARD.encode(rgb),
        }),
    }
}

fn healthcheck(cfg: &Config) -> Value {
    let mut render = json!({
        "name": RENDER,
        "description": "Path-trace one tile of a user-scripted scene",
        // 5xx responses (including Cloudflare's own, e.g. CPU-limit kills under load) are
        // retried with exponential backoff: 2s, 4s, 8s, 16s, then every 30s, so retries
        // don't land in the same burst that caused the failure. 4xx are not retried.
        "tasks": [{
            "readableId": RENDER, "action": RENDER_ACTION, "timeout": "300s",
            "retries": 8, "backoffFactor": 2.0, "backoffMaxSeconds": 30,
        }],
    });
    if let Some(max_runs) = cfg.max_concurrent_tiles {
        // One group for every tile of every render: a global in-flight cap.
        render["concurrencyArr"] =
            json!([{ "expression": "'tiles'", "maxRuns": max_runs, "limitStrategy": "GROUP_ROUND_ROBIN" }]);
    }
    let validate = json!({
        "name": VALIDATE,
        "description": "Check that a shape script parses",
        "tasks": [{ "readableId": VALIDATE, "action": VALIDATE_ACTION, "timeout": "30s", "retries": 2 }],
    });
    json!({
        "workflows": [render, validate],
        "durable": { "supported": false },
        "runtime": { "name": cfg.runtime_name, "sdkVersion": env!("CARGO_PKG_VERSION") },
    })
}
