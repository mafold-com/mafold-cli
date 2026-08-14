//! RPC to the Mafold API — the sync engine's REST layer, IN THE CORE.
//! `POST {base}/{method}` with a Bearer token + JSON body, unwrapping the bot-API
//! `{ok, result}` envelope. One signature, two transports: native = reqwest,
//! wasm = gloo-net (browser fetch).
//!
//! Two error surfaces:
//! - `rpc` (legacy, `Result<_, String>`): kept verbatim for the existing FFI
//!   exports (`lib.rs::rpc`, `set_language`) — the flat string collapses every
//!   failure class.
//! - `rpc_ex` (`Result<_, RpcError>`): structured — callers can tell a
//!   CONNECT-phase failure (request never left the machine → blind retry is
//!   safe; the botCreateDraft retry in mafold-cli depends on this) from an
//!   api-level `{ok:false}` (the RAW envelope is preserved so clients can parse
//!   `error_code`/`description` for typed errors, e.g. web's `ApiError`).
//!
//! It also carries a plain [`http`] helper for the ONE thing that is not the
//! Mafold API: talking to a provider's MCP server (see `mcp.rs`). That traffic
//! has no `{ok, result}` envelope, no Mafold token, and a different host per
//! call — but it wants the same two-transport split and the same pooled native
//! client, so it lives here rather than in a near-duplicate module.

/// Structured RPC failure. See the module doc for why each class exists.
#[derive(Debug, Clone)]
pub enum RpcError {
    /// Rejected before any I/O: the method name isn't in the api's route table
    /// (`methods::KNOWN_METHODS`). A typo caught at the core boundary instead
    /// of a confusing server 404.
    UnknownMethod(String),
    /// TCP/TLS connect phase failed — the server saw nothing, retrying blindly
    /// cannot duplicate anything.
    Connect(String),
    /// Any other transport failure (timeout, dropped response, non-JSON reply).
    /// NOT retry-safe for non-idempotent calls: the request may have landed.
    Transport(String),
    /// The server answered `{ok:false}` — carries the RAW envelope text so the
    /// caller can extract `error_code` / `description` / `error.message`.
    Api(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::UnknownMethod(m) => write!(f, "unknown method: {m}"),
            RpcError::Connect(e) => write!(f, "connect failed: {e}"),
            RpcError::Transport(e) => write!(f, "transport failed: {e}"),
            RpcError::Api(env) => {
                // Prefer the human `description` from the envelope when present.
                let desc = serde_json::from_str::<serde_json::Value>(env)
                    .ok()
                    .and_then(|v| {
                        v.get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                            .or_else(|| {
                                v.get("error").and_then(serde_json::Value::as_str).map(String::from)
                            })
                    });
                write!(f, "{}", desc.unwrap_or_else(|| env.clone()))
            }
        }
    }
}
impl std::error::Error for RpcError {}

/// Turn one HTTP reply into a result, using the STATUS as well as the body.
///
/// The status matters because the most common non-JSON reply has no body at all:
/// a route the server doesn't serve. Axum answers an unknown path with a bare
/// `404` and zero bytes, so parsing the body alone produced
/// `non-JSON reply:` — a message with nothing after the colon, naming neither
/// the method nor the status. That is the exact shape a client sees whenever it
/// ships ahead of the api, which is now the normal case (web deploys to
/// Cloudflare and the cli self-updates in seconds; the api rides a tag train),
/// so it deserves to say so in words instead of looking like corruption.
fn unwrap_envelope_ex(status: u16, method: &str, text: String) -> Result<String, RpcError> {
    // Not JSON at all (e.g. axum's plain-text 422 param rejection, or an empty
    // 404 body): surface the server's own words when it had any, and the status
    // when it didn't.
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
        let snippet: String = text.trim().chars().take(200).collect();
        RpcError::Transport(match (status, snippet.is_empty()) {
            (404, _) => format!(
                "{method}: this server has no such method (404) — it is older than this client"
            ),
            (_, true) => format!("{method}: HTTP {status} with an empty body"),
            (_, false) => format!("{method}: HTTP {status} — {snippet}"),
        })
    })?;
    if v.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(v.get("result").map(|r| r.to_string()).unwrap_or_else(|| "null".into()))
    } else {
        Err(RpcError::Api(text))
    }
}

/// Legacy flat-string surface (existing FFI callers). Same behaviour as before:
/// api errors collapse to their description string.
pub async fn rpc(base: &str, token: &str, method: &str, body: &str) -> Result<String, String> {
    rpc_ex(base, token, method, body).await.map_err(|e| e.to_string())
}

/// How long a browser RPC may go without an answer before we call it dead.
///
/// Generous on purpose. The longest legitimate synchronous hold anywhere in this
/// API is the 8s RPC-ack path (`mafold-api/src/main.rs`); inline queries cap at
/// 3s. Past a minute there is no server still thinking — there is a socket that
/// will never speak. Large transfers are NOT at risk: media upload uses a raw
/// multipart `fetch`, not this path.
#[cfg(target_arch = "wasm32")]
const WEB_RPC_TIMEOUT_MS: u32 = 60_000;

#[cfg(target_arch = "wasm32")]
pub async fn rpc_ex(base: &str, token: &str, method: &str, body: &str) -> Result<String, RpcError> {
    use futures::future::{select, Either};
    use gloo_net::http::Request;
    // Browser fetch doesn't expose the connect phase — every network failure is
    // Transport (the retry-safety split only matters to native daemons anyway).
    //
    // It also has NO timeout of its own. On a half-open connection (captive
    // portal, resumed laptop, a proxy that swallows the socket) the promise
    // neither resolves nor rejects: the caller waits forever, and a UI that
    // flipped to a pending state on the way in never gets the error that would
    // let it flip back. That is how a tapped Stop button stayed on "Stopping…"
    // indefinitely with nothing on screen to say the request had died. The
    // native client bounds its connect phase; this is the web's equivalent.
    let controller = web_sys::AbortController::new()
        .map_err(|_| RpcError::Transport("AbortController unavailable".into()))?;
    let signal = controller.signal();

    let request = Request::post(&format!("{base}/{method}"))
        .header("content-type", "application/json")
        .header("authorization", &format!("Bearer {token}"))
        .abort_signal(Some(&signal))
        .body(body.to_string())
        .map_err(|e| RpcError::Transport(e.to_string()))?;

    // Owned copy: the work future is `move`, so it can't borrow `method`.
    let method_for_err = method.to_string();
    // Headers AND body are inside the deadline: a response that starts and then
    // stalls mid-body hangs just as hard as one that never arrives.
    let work = Box::pin(async move {
        let resp = request.send().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        unwrap_envelope_ex(status, &method_for_err, text)
    });
    let deadline = Box::pin(gloo_timers::future::TimeoutFuture::new(WEB_RPC_TIMEOUT_MS));

    match select(work, deadline).await {
        Either::Left((out, _)) => out,
        Either::Right(((), _)) => {
            // Tear the socket down rather than leave it pending for the life of
            // the page — an abandoned fetch still holds a connection slot.
            controller.abort();
            Err(RpcError::Transport(format!(
                "{method}: no response in {}s — the connection appears to be down",
                WEB_RPC_TIMEOUT_MS / 1000
            )))
        }
    }
}

// One shared client across all RPCs so connection/TLS sessions POOL (a fresh
// `Client::new()` per call throws the pool away and re-does the TLS handshake).
// Bound only the CONNECT phase: an overall timeout would cut off long streams
// (self-update downloads) that callers tunnel through the same client.
#[cfg(not(target_arch = "wasm32"))]
fn client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn rpc_ex(base: &str, token: &str, method: &str, body: &str) -> Result<String, RpcError> {
    let resp = client()
        .post(format!("{base}/{method}"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                RpcError::Connect(e.to_string())
            } else {
                RpcError::Transport(e.to_string())
            }
        })?;
    let status = resp.status().as_u16();
    unwrap_envelope_ex(
        status,
        method,
        resp.text().await.map_err(|e| RpcError::Transport(e.to_string()))?,
    )
}

// ── plain HTTP, for hosts that are not the Mafold API ──────────────────────

/// One reply, reduced to what callers here read.
pub struct HttpReply {
    pub status: u16,
    /// Header names lowercased.
    ///
    /// On the web this holds only what the server listed in
    /// `Access-Control-Expose-Headers`; the browser hides everything else, and
    /// there is no way to tell "hidden" from "not sent". Both MCP servers we
    /// speak to expose nothing, so on wasm this is effectively empty — which is
    /// why `mcp.rs` must work without a session id rather than treat one as
    /// guaranteed.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpReply {
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn http_post(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<HttpReply, RpcError> {
    let mut req = client().post(url).body(body.to_string());
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_connect() {
            RpcError::Connect(e.to_string())
        } else {
            RpcError::Transport(e.to_string())
        }
    })?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_string())))
        .collect();
    let body = resp.text().await.map_err(|e| RpcError::Transport(e.to_string()))?;
    Ok(HttpReply { status, headers, body })
}

#[cfg(target_arch = "wasm32")]
pub async fn http_post(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<HttpReply, RpcError> {
    use futures::future::{select, Either};
    use gloo_net::http::Request;

    let controller = web_sys::AbortController::new()
        .map_err(|_| RpcError::Transport("AbortController unavailable".into()))?;
    let signal = controller.signal();

    let mut builder = Request::post(url).abort_signal(Some(&signal));
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let request = builder
        .body(body.to_string())
        .map_err(|e| RpcError::Transport(e.to_string()))?;

    let work = Box::pin(async move {
        let resp = request.send().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        let status = resp.status();
        let headers = resp
            .headers()
            .entries()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        let body = resp.text().await.map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(HttpReply { status, headers, body })
    });
    let deadline = Box::pin(gloo_timers::future::TimeoutFuture::new(WEB_RPC_TIMEOUT_MS));

    match select(work, deadline).await {
        Either::Left((out, _)) => out,
        Either::Right(((), _)) => {
            controller.abort();
            Err(RpcError::Transport(format!(
                "no response in {}s from {url}",
                WEB_RPC_TIMEOUT_MS / 1000
            )))
        }
    }
}

// ── streaming HTTP, for provider surfaces that answer as SSE ───────────────
//
// Same two-transport split as everything above, with one honest asymmetry: the
// browser's `gloo` fetch surfaces the body only when it is complete, so on wasm
// a "stream" is the whole body delivered as one chunk at the end. That is a
// degradation, not a lie — callers see identical types and the same terminal
// behaviour, they just see it later. The devices that actually answer long
// streaming calls are native daemons; the wasm arm exists so the core keeps
// compiling everywhere rather than to make a browser a good streaming device.

/// One in-flight streaming reply. `next()` yields raw body bytes as they
/// arrive (native) or the whole body once (wasm), then `None`.
pub struct StreamingReply {
    pub status: u16,
    #[cfg(not(target_arch = "wasm32"))]
    inner: Option<reqwest::Response>,
    #[cfg(target_arch = "wasm32")]
    inner: Option<Vec<u8>>,
}

impl StreamingReply {
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(resp) = self.inner.as_mut() else { return Ok(None) };
            match resp.chunk().await {
                Ok(Some(b)) => Ok(Some(b.to_vec())),
                Ok(None) => {
                    self.inner = None;
                    Ok(None)
                }
                Err(e) => {
                    self.inner = None;
                    Err(RpcError::Transport(e.to_string()))
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self.inner.take())
        }
    }
}

/// POST and hold the reply open. On a NON-success status the caller usually
/// wants the error body in one piece — use `next()` all the same; error bodies
/// are small and arrive in one or two chunks.
#[cfg(not(target_arch = "wasm32"))]
pub async fn http_post_streaming(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<StreamingReply, RpcError> {
    let mut req = client().post(url).body(body.to_string());
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_connect() {
            RpcError::Connect(e.to_string())
        } else {
            RpcError::Transport(e.to_string())
        }
    })?;
    Ok(StreamingReply {
        status: resp.status().as_u16(),
        inner: Some(resp),
    })
}

#[cfg(target_arch = "wasm32")]
pub async fn http_post_streaming(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<StreamingReply, RpcError> {
    let reply = http_post(url, headers, body).await?;
    Ok(StreamingReply {
        status: reply.status,
        inner: Some(reply.body.into_bytes()),
    })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testutil::{ok, spawn_mock};

    // ── envelope unwrap (pure) ──

    #[test]
    fn envelope_ok_extracts_result() {
        let r = unwrap_envelope_ex(200, "getChats", r#"{"ok":true,"result":{"a":1}}"#.into()).unwrap();
        assert_eq!(r, r#"{"a":1}"#);
        // ok with no result → the literal "null" (callers JSON.parse it).
        assert_eq!(unwrap_envelope_ex(200, "getChats", r#"{"ok":true}"#.into()).unwrap(), "null");
    }

    #[test]
    fn envelope_err_is_api_with_raw_text() {
        let raw = r#"{"ok":false,"error_code":404,"description":"conversation not found"}"#;
        match unwrap_envelope_ex(404, "getChat", raw.into()) {
            Err(RpcError::Api(env)) => assert_eq!(env, raw, "Api must carry the RAW envelope"),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn envelope_non_json_is_transport_snippet() {
        // axum's plain-text 422 param rejection is the real-world case here.
        let e = unwrap_envelope_ex(422, "getUser", "Failed to deserialize the JSON body: missing field `user_id`".into())
            .unwrap_err();
        match e {
            RpcError::Transport(m) => {
                assert!(m.contains("getUser"), "the method must be named: {m}");
                assert!(m.contains("422"), "the status must be named: {m}");
                assert!(m.contains("missing field"), "server's own words must surface: {m}");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        // Snippet is capped at 200 chars so a huge HTML error page can't flood logs.
        let big = "x".repeat(5000);
        let RpcError::Transport(m) = unwrap_envelope_ex(500, "getChats", big).unwrap_err() else { panic!() };
        assert!(m.len() <= 260, "snippet must stay bounded: {}", m.len());
    }

    /// The client-newer-than-server case, which is now routine: web ships to
    /// Cloudflare and the cli self-updates in seconds while the api rides a tag
    /// train. Axum answers an unknown route with a bare 404 and NO body, so the
    /// old body-only path produced `non-JSON reply:` with nothing after the
    /// colon — indistinguishable from corruption, and naming neither the method
    /// nor the status.
    #[test]
    fn an_empty_404_says_the_server_lacks_the_method() {
        let RpcError::Transport(m) = unwrap_envelope_ex(404, "listConnections", String::new()).unwrap_err()
        else {
            panic!("expected Transport")
        };
        assert!(m.contains("listConnections"), "{m}");
        assert!(m.contains("404"), "{m}");
        assert!(m.contains("older than this client"), "{m}");
        assert!(!m.ends_with(": "), "must not trail off into nothing: {m:?}");
    }

    /// A body-less non-404 (a bare 502 from a proxy) still has to say something.
    #[test]
    fn an_empty_body_still_names_the_status() {
        let RpcError::Transport(m) = unwrap_envelope_ex(502, "getChats", "   ".into()).unwrap_err()
        else {
            panic!("expected Transport")
        };
        assert!(m.contains("502") && m.contains("getChats"), "{m}");
    }

    #[test]
    fn rpc_error_display_formats() {
        assert_eq!(RpcError::UnknownMethod("sendMesage".into()).to_string(), "unknown method: sendMesage");
        assert!(RpcError::Connect("refused".into()).to_string().starts_with("connect failed: "));
        assert!(RpcError::Transport("t".into()).to_string().starts_with("transport failed: "));
        // Api Display prefers `description`, falls back to a string `error`, then raw.
        assert_eq!(
            RpcError::Api(r#"{"ok":false,"description":"nope"}"#.into()).to_string(),
            "nope"
        );
        assert_eq!(RpcError::Api(r#"{"ok":false,"error":"bad"}"#.into()).to_string(), "bad");
        assert_eq!(RpcError::Api("garbage".into()).to_string(), "garbage");
    }

    // ── transport classification (real sockets) ──

    /// The retry-safety contract mafold-cli's botCreateDraft depends on:
    /// a CONNECT-phase failure (server saw nothing) classifies as Connect.
    #[tokio::test]
    async fn connect_refused_classified_as_connect() {
        // Bind then DROP a listener — the port is now (very likely) refusing.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let e = rpc_ex(&format!("http://{addr}"), "t", "getMe", "{}").await.unwrap_err();
        assert!(matches!(e, RpcError::Connect(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn garbage_reply_classified_as_transport() {
        let mock = spawn_mock(vec![(200, "<html>cf error page</html>".into())]);
        let e = rpc_ex(&mock.base, "t", "getMe", "{}").await.unwrap_err();
        assert!(matches!(e, RpcError::Transport(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn rpc_legacy_flattens_to_string() {
        let mock = spawn_mock(vec![(200, r#"{"ok":false,"error_code":401,"description":"unauthorized"}"#.into())]);
        let e = rpc(&mock.base, "t", "getMe", "{}").await.unwrap_err();
        assert_eq!(e, "unauthorized");
        // And the ok path round-trips the result through the legacy surface too.
        let mock = spawn_mock(vec![ok(r#"{"fine":true}"#)]);
        assert_eq!(rpc(&mock.base, "t", "getMe", "{}").await.unwrap(), r#"{"fine":true}"#);
    }
}
