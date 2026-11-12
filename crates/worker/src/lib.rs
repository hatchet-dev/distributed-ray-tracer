//! Cloudflare Worker adapter: moves requests into `endpoint::handle` and back out, loading
//! task scripts from the durable stream topics the app Worker publishes them to.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::pin::pin;
use std::rc::Rc;
use std::time::Duration;

use endpoint::{handle, scripts, Config, Scripts, HEALTHCHECK_PATH, TRIGGER_PATH};
use futures_util::future::{select, Either};
use futures_util::StreamExt;
use worker::*;

/// Scripts kept in this isolate's memory, most recent last: a render's tiles share one script,
/// so most tiles skip the engine round trip. Bounded, since scripts can be megabytes.
const CACHED_SCRIPTS: usize = 4;
/// A script read normally takes well under a second. A stalled one (the engine connection can
/// hang for minutes before failing) is abandoned after this, so the tile fails fast and Hatchet
/// retries it instead of the render waiting on it.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(20);

thread_local! {
    static CACHE: RefCell<VecDeque<(String, Rc<String>)>> = const { RefCell::new(VecDeque::new()) };
}

/// Reads `script-<id>` with the engine's Subscribe call (Connect over HTTP) and hangs up after
/// the first message.
struct StreamScripts {
    token: Option<String>,
    engine: Option<String>,
}

impl StreamScripts {
    async fn fetch(&self, id: &str) -> std::result::Result<String, String> {
        let controller = AbortController::default();
        let read = pin!(self.subscribe(id, controller.signal()));
        match select(read, pin!(Delay::from(SCRIPT_TIMEOUT))).await {
            Either::Left((result, _)) => result,
            Either::Right(_) => {
                controller.abort();
                Err(format!("timed out after {}s", SCRIPT_TIMEOUT.as_secs()))
            }
        }
    }

    async fn subscribe(&self, id: &str, signal: AbortSignal) -> std::result::Result<String, String> {
        let token = self.token.as_deref().ok_or("HATCHET_CLIENT_TOKEN is not set")?;
        let engine = self.engine.as_deref().ok_or("the token has no engine address; set HATCHET_ENGINE_URL")?;
        let err = |e: Error| e.to_string();
        let headers = Headers::new();
        for (name, value) in scripts::SUBSCRIBE_HEADERS {
            headers.set(name, value).map_err(err)?;
        }
        headers.set("Authorization", &format!("Bearer {token}")).map_err(err)?;
        let body = js_sys::Uint8Array::from(scripts::subscribe_body(&scripts::topic(id)).as_slice());
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_headers(headers).with_body(Some(body.into()));
        let request = Request::new_with_init(&format!("{engine}{}", scripts::SUBSCRIBE_PATH), &init).map_err(err)?;
        let mut response = Fetch::Request(request).send_with_signal(&signal).await.map_err(err)?;
        if response.status_code() != 200 {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("subscribe answered {}: {text}", response.status_code()));
        }

        let mut stream = response.stream().map_err(err)?;
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend(chunk.map_err(err)?);
            if let scripts::Read::Payload(payload) = scripts::first_payload(&buf)? {
                return String::from_utf8(payload).map_err(|_| "the script is not UTF-8".to_string());
            }
        }
        Err("the stream ended before the script arrived".into())
    }
}

impl Scripts for StreamScripts {
    async fn publish(&self, body: Vec<u8>) -> std::result::Result<(), String> {
        let token = self.token.as_deref().ok_or("HATCHET_CLIENT_TOKEN is not set")?;
        let engine = self.engine.as_deref().ok_or("the token has no engine address; set HATCHET_ENGINE_URL")?;
        let err = |e: Error| e.to_string();
        let headers = Headers::new();
        for (name, value) in scripts::PUBLISH_HEADERS {
            headers.set(name, value).map_err(err)?;
        }
        headers.set("Authorization", &format!("Bearer {token}")).map_err(err)?;
        let body = js_sys::Uint8Array::from(body.as_slice());
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_headers(headers).with_body(Some(body.into()));
        let request = Request::new_with_init(&format!("{engine}{}", scripts::PUBLISH_PATH), &init).map_err(err)?;
        let mut response = Fetch::Request(request).send().await.map_err(err)?;
        let status = response.status_code();
        if status != 200 {
            let text = response.bytes().await.unwrap_or_default();
            return Err(scripts::call_error(status, &text));
        }
        Ok(())
    }

    async fn get(&self, id: &str) -> std::result::Result<String, String> {
        let cached = CACHE.with_borrow(|c| c.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone()));
        if let Some(script) = cached {
            return Ok((*script).clone());
        }
        let script = self.fetch(id).await?;
        CACHE.with_borrow_mut(|c| {
            c.push_back((id.to_string(), Rc::new(script.clone())));
            if c.len() > CACHED_SCRIPTS {
                c.pop_front();
            }
        });
        Ok(script)
    }
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    if path != HEALTHCHECK_PATH && path != TRIGGER_PATH {
        return Response::error("Not Found", 404);
    }
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    let var = |k: &str| env.secret(k).or_else(|_| env.var(k)).map(|v| v.to_string()).ok();
    let Some(secret) = var("HATCHET_SIGNING_SECRET") else {
        return Response::error("HATCHET_SIGNING_SECRET is not set", 500);
    };
    let endpoint_id = var("HATCHET_ENDPOINT_ID");
    let cfg = Config {
        signing_secret: &secret,
        endpoint_id: endpoint_id.as_deref(),
        runtime_name: "rust-cloudflare-workers",
        serve_render: endpoint::serves_render(var("SERVE_TASKS").as_deref()),
    };
    let signature = req.headers().get("X-Hatchet-Signature")?.unwrap_or_default();
    let body = req.bytes().await?;
    let now = (Date::now().as_millis() / 1000) as i64;

    let token = var("HATCHET_CLIENT_TOKEN");
    let engine = var("HATCHET_ENGINE_URL").or_else(|| token.as_deref().and_then(scripts::engine_url));
    let store = StreamScripts { token, engine };
    let reply = handle(&path, &signature, &body, &cfg, now, &store).await;
    Ok(Response::from_json(&reply.body)?.with_status(reply.status))
}
