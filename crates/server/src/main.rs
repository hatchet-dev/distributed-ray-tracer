//! Native server adapter: the same endpoint as the Cloudflare tile Worker (`crates/worker`), as
//! a plain HTTP server for a container. Moves requests into `endpoint::handle` and back out,
//! loading task scripts from the durable stream topics the app publishes them to.
//!
//! Environment (the same names the Worker reads):
//!   PORT                    where to listen (default 8080)
//!   HATCHET_SIGNING_SECRET  required, >= 32 chars, also given to Hatchet when registering
//!   HATCHET_CLIENT_TOKEN    tenant API token: reads shape scripts from their topics
//!   HATCHET_ENDPOINT_ID     optional, rejects requests signed for another endpoint
//!   HATCHET_ENGINE_URL      optional, the engine's base URL; defaults to the token's address
//!   MAX_CONCURRENT_TILES    optional, declared to Hatchet as the tiles' concurrency limit
//!
//! Tiles render on blocking threads, so many can run at once across the machine's cores.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use endpoint::{handle, scripts, Config, Scripts, HEALTHCHECK_PATH, TRIGGER_PATH};
use futures_util::StreamExt;

/// Scripts kept in memory, most recent last: a render's tiles share one script, so most tiles
/// skip the engine round trip. Bounded, since scripts can be megabytes.
const CACHED_SCRIPTS: usize = 8;
/// A stalled script read is abandoned after this, so the tile fails fast and Hatchet retries it.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(20);

struct Settings {
    signing_secret: String,
    endpoint_id: Option<String>,
    max_concurrent_tiles: Option<u32>,
    token: Option<String>,
    engine: Option<String>,
}

struct App {
    settings: Settings,
    http: reqwest::Client,
    cache: Mutex<VecDeque<(String, Arc<String>)>>,
}

/// Reads `script-<id>` with the engine's Subscribe call (Connect over HTTP) and hangs up after
/// the first message.
struct StreamScripts<'a>(&'a App);

impl StreamScripts<'_> {
    async fn fetch(&self, id: &str) -> Result<String, String> {
        match tokio::time::timeout(SCRIPT_TIMEOUT, self.subscribe(id)).await {
            Ok(result) => result,
            Err(_) => Err(format!("timed out after {}s", SCRIPT_TIMEOUT.as_secs())),
        }
    }

    async fn subscribe(&self, id: &str) -> Result<String, String> {
        let s = &self.0.settings;
        let token = s.token.as_deref().ok_or("HATCHET_CLIENT_TOKEN is not set")?;
        let engine = s.engine.as_deref().ok_or("the token has no engine address; set HATCHET_ENGINE_URL")?;
        let mut request = self
            .0
            .http
            .post(format!("{engine}{}", scripts::SUBSCRIBE_PATH))
            .bearer_auth(token)
            .body(scripts::subscribe_body(&scripts::topic(id)));
        for (name, value) in scripts::SUBSCRIBE_HEADERS {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status() != 200 {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("subscribe answered {status}: {text}"));
        }
        // Dropping the response (on return) hangs up the subscription.
        let mut stream = response.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
            if let scripts::Read::Payload(payload) = scripts::first_payload(&buf)? {
                return String::from_utf8(payload).map_err(|_| "the script is not UTF-8".to_string());
            }
        }
        Err("the stream ended before the script arrived".into())
    }
}

impl Scripts for StreamScripts<'_> {
    async fn get(&self, id: &str) -> Result<String, String> {
        let cached = self.0.cache.lock().unwrap().iter().find(|(k, _)| k == id).map(|(_, v)| v.clone());
        if let Some(script) = cached {
            return Ok((*script).clone());
        }
        let script = self.fetch(id).await?;
        let mut cache = self.0.cache.lock().unwrap();
        if !cache.iter().any(|(k, _)| k == id) {
            cache.push_back((id.to_string(), Arc::new(script.clone())));
            if cache.len() > CACHED_SCRIPTS {
                cache.pop_front();
            }
        }
        Ok(script)
    }
}

async fn hatchet(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let signature = headers.get("X-Hatchet-Signature").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default();
    let runtime = tokio::runtime::Handle::current();
    // Rendering is CPU-bound and the endpoint's futures aren't Send: run the whole request on a
    // blocking thread, driving its script fetch with this runtime.
    let reply = tokio::task::spawn_blocking(move || {
        let s = &app.settings;
        let cfg = Config {
            signing_secret: &s.signing_secret,
            endpoint_id: s.endpoint_id.as_deref(),
            runtime_name: "rust-native",
            max_concurrent_tiles: s.max_concurrent_tiles,
        };
        runtime.block_on(handle(&path, &signature, &body, &cfg, now, &StreamScripts(&app)))
    })
    .await;
    match reply {
        Ok(reply) => (StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(reply.body)).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, format!("the request failed: {err}")).into_response(),
    }
}

#[tokio::main]
async fn main() {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(signing_secret) = var("HATCHET_SIGNING_SECRET") else {
        eprintln!("HATCHET_SIGNING_SECRET is not set");
        std::process::exit(1);
    };
    let token = var("HATCHET_CLIENT_TOKEN");
    let engine = var("HATCHET_ENGINE_URL").or_else(|| token.as_deref().and_then(scripts::engine_url));
    let settings = Settings {
        signing_secret,
        endpoint_id: var("HATCHET_ENDPOINT_ID"),
        max_concurrent_tiles: var("MAX_CONCURRENT_TILES").and_then(|v| v.parse().ok()),
        token,
        engine,
    };
    let app = Arc::new(App { settings, http: reqwest::Client::new(), cache: Mutex::default() });
    let router = Router::new()
        .route(HEALTHCHECK_PATH, post(hatchet))
        .route(TRIGGER_PATH, post(hatchet))
        // For the platform's own health probe.
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app);

    let port = var("PORT").unwrap_or_else(|| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.expect("bind");
    println!("listening on :{port}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("serve");
}
