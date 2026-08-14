//! The `codex-responses` native driver — the device half of running a model
//! turn on the user's own Codex (ChatGPT) subscription.
//!
//! Split of knowledge, stated once: **credentials live here, protocol lives in
//! the server brain.** The brain builds the full Responses-API request body and
//! parses the SSE it gets back; this driver's whole job is what only a vault
//! device can do — open the payload, put the bearer token and account id on the
//! wire, keep the grant renewed, and relay the byte stream. It forwards SSE
//! **lines** verbatim and never interprets them, so a protocol change is a
//! server-side edit, not a fleet of daemons to update.
//!
//! The identity headers are the Codex CLI's own (`codex-tui/<version>`),
//! because the ChatGPT-internal endpoint serves exactly that client family and
//! validates that `originator` matches the User-Agent's first segment. The
//! version constant matters operationally: the upstream sheds load by client
//! version (stale versions get `server_is_overloaded` first), so it should
//! track the current Codex CLI release when it drifts far behind.

use serde_json::{json, Map, Value};

use crate::connections::Runtime;
use crate::net;
use mafold_types::connections::{codex, ProviderSpec};

const ORIGINATOR: &str = "codex-tui";
/// Track the shipping Codex CLI. Falling several releases behind moves every
/// call into the first-to-shed bucket under load (HTTP 200 + in-stream
/// `server_is_overloaded`), which reads as flakiness rather than as this line.
const CLIENT_VERSION: &str = "0.146.0";

/// Abort a call when the upstream goes silent for this long. The brain holds
/// its own (shorter) idle deadline; this one only exists so a dead TCP peer
/// can't pin the claimed call — and the vault runtime — forever.
#[cfg(not(target_arch = "wasm32"))]
const READ_IDLE_MS: u64 = 180_000;

/// Batch relayed lines so a chatty token stream doesn't become one RPC per
/// token: flush on this clock, or earlier when the buffer grows real.
#[cfg(not(target_arch = "wasm32"))]
const FLUSH_EVERY_MS: u64 = 150;
#[cfg(not(target_arch = "wasm32"))]
const FLUSH_BYTES: usize = 8 * 1024;

fn ua() -> String {
    let os = match std::env::consts::OS {
        "macos" => "Mac OS 26.0",
        "windows" => "Windows 11",
        _ => "Ubuntu 22.4.0",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        a => a,
    };
    format!("{ORIGINATOR}/{CLIENT_VERSION} ({os}; {arch}) xterm-256color")
}

fn headers(payload: &Map<String, Value>, session: &str) -> Result<Vec<(String, String)>, String> {
    let get = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let access = get("access_token");
    if access.is_empty() {
        return Err("connection has no access token".into());
    }
    let account = get("account_id");
    if account.is_empty() {
        return Err(
            "connection has no ChatGPT account id — re-link it: \
             mafold connection add codex --provider codex-oauth --oauth"
                .into(),
        );
    }
    Ok(vec![
        ("authorization".into(), format!("Bearer {access}")),
        ("chatgpt-account-id".into(), account),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
        ("openai-beta".into(), "responses=experimental".into()),
        ("originator".into(), ORIGINATOR.into()),
        ("version".into(), CLIENT_VERSION.into()),
        ("user-agent".into(), ua()),
        ("session_id".into(), session.into()),
        ("conversation_id".into(), session.into()),
    ])
}

/// Run one relayed Codex call. `stream=true` posts SSE lines back as
/// `answerConnectionCallChunk` batches while the model generates; the returned
/// value is only the terminal summary either way.
pub(crate) async fn run(
    rt: &Runtime,
    call_id: &str,
    name: &str,
    conn: &Value,
    spec: &'static ProviderSpec,
    params: &Value,
    stream: bool,
) -> Result<Value, String> {
    run_at(codex::RESPONSES_URL, rt, call_id, name, conn, spec, params, stream).await
}

/// The URL is a parameter so tests can stand in for the backend; production
/// has exactly one caller and it passes the constant.
#[allow(clippy::too_many_arguments)]
async fn run_at(
    url: &str,
    rt: &Runtime,
    call_id: &str,
    name: &str,
    conn: &Value,
    spec: &'static ProviderSpec,
    params: &Value,
    stream: bool,
) -> Result<Value, String> {
    let body = params
        .get("body")
        .filter(|b| b.is_object())
        .ok_or("codex call carries no request body")?
        .to_string();
    let session = params
        .get("session")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(call_id);

    // Proactive renewal first (expires_at is in the payload); reactive 401
    // renewal below catches the imports that carry no expiry to be proactive
    // about.
    let mut payload = rt.refreshed_payload(name, conn, spec).await?;

    for attempt in 0..2u8 {
        let hs = headers(&payload, session)?;
        let mut reply = net::http_post_streaming(url, &hs, &body)
            .await
            .map_err(|e| e.to_string())?;

        if reply.status == 401 && attempt == 0 {
            payload = rt.renew(name, conn, spec, &payload).await.map_err(|e| {
                format!(
                    "Codex refused this credential and it couldn't be refreshed here ({e}) — \
                     re-link it: mafold connection add {name} --provider codex-oauth --oauth"
                )
            })?;
            continue;
        }
        if reply.status >= 400 {
            let mut err_body: Vec<u8> = Vec::new();
            while let Ok(Some(b)) = reply.next().await {
                err_body.extend_from_slice(&b);
                if err_body.len() > 16 * 1024 {
                    break;
                }
            }
            let snippet: String = String::from_utf8_lossy(&err_body).chars().take(400).collect();
            return Err(format!("codex backend answered HTTP {}: {snippet}", reply.status));
        }

        return relay_body(rt, call_id, reply, stream).await;
    }
    unreachable!("the 401 arm either returned or retried");
}

/// Forward the reply body. Only COMPLETE lines travel — a UTF-8 char or an SSE
/// `data:` line split across TCP frames must never reach the parser halved, so
/// the unterminated tail carries over between chunks (same discipline as the
/// api's own SSE reader).
#[cfg(not(target_arch = "wasm32"))]
async fn relay_body(
    rt: &Runtime,
    call_id: &str,
    mut reply: net::StreamingReply,
    stream: bool,
) -> Result<Value, String> {
    use std::time::Duration;

    let status = reply.status;
    let mut carry: Vec<u8> = Vec::new();
    let mut pending = String::new();
    let mut collected = String::new();
    let mut seq: u64 = 0;
    let mut last_flush = tokio::time::Instant::now();

    loop {
        // While lines are pending, wake on the flush clock too — otherwise a
        // model pause right after a burst would hold that burst hostage.
        let next = if stream && !pending.is_empty() {
            match tokio::time::timeout_at(
                last_flush + Duration::from_millis(FLUSH_EVERY_MS),
                reply.next(),
            )
            .await
            {
                Ok(r) => Some(r),
                Err(_) => None, // flush deadline hit
            }
        } else {
            match tokio::time::timeout(Duration::from_millis(READ_IDLE_MS), reply.next()).await {
                Ok(r) => Some(r),
                Err(_) => return Err("codex backend went silent mid-stream".into()),
            }
        };

        let mut done = false;
        if let Some(r) = next {
            match r.map_err(|e| e.to_string())? {
                Some(bytes) => {
                    carry.extend_from_slice(&bytes);
                    while let Some(pos) = carry.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = carry.drain(..=pos).collect();
                        let text = String::from_utf8_lossy(&line);
                        if stream {
                            pending.push_str(&text);
                        } else {
                            collected.push_str(&text);
                        }
                    }
                }
                None => done = true,
            }
        }

        let flush_due = done
            || pending.len() >= FLUSH_BYTES
            || last_flush.elapsed() >= Duration::from_millis(FLUSH_EVERY_MS);
        if stream && !pending.is_empty() && flush_due {
            let text = std::mem::take(&mut pending);
            rt.rpc(
                "answerConnectionCallChunk",
                json!({ "call_id": call_id, "seq": seq, "text": text }),
            )
            .await?;
            seq += 1;
            last_flush = tokio::time::Instant::now();
        }

        if done {
            // A final unterminated tail still belongs to the caller.
            if !carry.is_empty() {
                let text = String::from_utf8_lossy(&carry).to_string();
                if stream {
                    rt.rpc(
                        "answerConnectionCallChunk",
                        json!({ "call_id": call_id, "seq": seq, "text": text }),
                    )
                    .await?;
                } else {
                    collected.push_str(&text);
                }
            }
            return Ok(if stream {
                json!({ "status": status, "chunks": seq })
            } else {
                json!({ "status": status, "body": collected })
            });
        }
    }
}

/// wasm arm: `StreamingReply` already degraded to whole-body-at-once, so relay
/// it as one chunk. A browser is not a good streaming device; it is merely a
/// CORRECT one.
#[cfg(target_arch = "wasm32")]
async fn relay_body(
    rt: &Runtime,
    call_id: &str,
    mut reply: net::StreamingReply,
    stream: bool,
) -> Result<Value, String> {
    let status = reply.status;
    let mut all: Vec<u8> = Vec::new();
    while let Some(b) = reply.next().await.map_err(|e| e.to_string())? {
        all.extend_from_slice(&b);
    }
    let text = String::from_utf8_lossy(&all).to_string();
    if stream {
        rt.rpc(
            "answerConnectionCallChunk",
            json!({ "call_id": call_id, "seq": 0, "text": text }),
        )
        .await?;
        Ok(json!({ "status": status, "chunks": 1 }))
    } else {
        Ok(json!({ "status": status, "body": text }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn identity_headers_pair_originator_with_the_ua_first_segment() {
        let hs = headers(
            &map(json!({ "access_token": "at", "account_id": "acc-1" })),
            "sess-1",
        )
        .unwrap();
        let get = |k: &str| hs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        // The upstream rejects a mismatched pair outright, so this is the
        // contract, not a style preference.
        let ua = get("user-agent");
        assert!(ua.starts_with(&format!("{ORIGINATOR}/")), "{ua}");
        assert_eq!(get("originator"), ORIGINATOR);
        assert_eq!(get("version"), CLIENT_VERSION);
        assert_eq!(get("authorization"), "Bearer at");
        assert_eq!(get("chatgpt-account-id"), "acc-1");
        assert_eq!(get("accept"), "text/event-stream");
        assert_eq!(get("openai-beta"), "responses=experimental");
        assert_eq!(get("session_id"), "sess-1");
    }

    #[test]
    fn a_payload_without_account_id_says_how_to_fix_it() {
        let err = headers(&map(json!({ "access_token": "at" })), "s").unwrap_err();
        assert!(err.contains("--provider codex-oauth"), "{err}");
    }

    #[test]
    fn a_payload_without_access_token_is_refused_before_any_network() {
        assert!(headers(&map(json!({ "account_id": "a" })), "s").is_err());
    }

    // ── the driver against a live (mock) wire ──

    #[cfg(not(target_arch = "wasm32"))]
    mod wire {
        use super::*;
        use crate::testutil::{ok, spawn_mock};
        use crate::vault::{self, Key};

        fn sealed_conn(umk: &Key, payload: Value) -> Value {
            let sealed = vault::seal_payload(umk, &payload.to_string());
            json!({
                "name": "codex",
                "provider": "codex-oauth",
                "blob": sealed.blob,
                "wrapped_dek": sealed.wrapped_dek,
            })
        }

        fn spec() -> &'static mafold_types::connections::ProviderSpec {
            mafold_types::connections::provider("codex-oauth").unwrap()
        }

        fn params() -> Value {
            json!({ "body": { "model": "gpt-5.6", "stream": true }, "session": "sess-1" })
        }

        /// The whole streaming leg on one real socket: sign → read SSE → relay
        /// complete lines as chunk RPCs → summarize. The mock serves responses
        /// per CONNECTION in order, so the codex POST and the api RPCs can
        /// share one server.
        #[tokio::test]
        async fn streams_sse_lines_back_as_chunk_rpcs() {
            let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\
                       data: {\"type\":\"response.completed\",\"response\":{}}\n";
            let mock = spawn_mock(vec![
                (200, sse.into()), // the codex POST
                ok("null"),        // chunk rpc(s) + final answer (last repeats)
            ]);
            let umk = Key::random();
            let conn = sealed_conn(&umk, json!({ "access_token": "at-1", "account_id": "acc-1" }));
            let rt = Runtime::new(&mock.base, "s_tok", umk);

            let out = run_at(
                &format!("{}/backend", mock.base),
                &rt, "call-7", "codex", &conn, spec(), &params(), true,
            )
            .await
            .expect("driver run");
            assert_eq!(out["status"], 200);

            // Request 0 = the signed backend POST.
            let post = mock.request(0);
            assert_eq!(post.path, "/backend");
            assert_eq!(post.header("authorization"), Some("Bearer at-1"));
            assert_eq!(post.header("chatgpt-account-id"), Some("acc-1"));
            assert_eq!(post.header("originator"), Some(ORIGINATOR));
            assert_eq!(post.header("session_id"), Some("sess-1"));
            assert!(post.body.contains("\"model\":\"gpt-5.6\""));

            // Then the relayed chunk carries the raw SSE lines, and the driver
            // never interpreted them.
            let reqs = mock.requests.lock().unwrap().clone();
            let chunk = reqs
                .iter()
                .find(|r| r.path == "/answerConnectionCallChunk")
                .expect("a chunk rpc");
            assert!(chunk.body.contains("call-7"));
            assert!(chunk.body.contains("response.output_text.delta"));
        }

        /// A 401 spends the refresh token ONCE, stores the rotated grant, and
        /// retries with the fresh access token — the exact sequence a device
        /// needs when it inherited a payload with no usable expiry.
        #[tokio::test]
        async fn a_401_refreshes_once_and_retries_with_the_new_token() {
            let sse = "data: {\"type\":\"response.completed\",\"response\":{}}\n";
            let mock = spawn_mock(vec![
                (401, "denied".into()),                                      // codex POST #1
                (200, r#"{"access_token":"at-new","expires_in":3600}"#.into()), // token endpoint
                ok("null"),                                                  // putConnection (store)
                (200, sse.into()),                                           // codex POST #2
                ok("null"),                                                  // chunk / answer rpcs
            ]);
            let umk = Key::random();
            let rt = Runtime::new(&mock.base, "s_tok", umk.clone());
            let conn = sealed_conn(
                &umk,
                json!({
                    "access_token": "at-old",
                    "account_id": "acc-1",
                    "refresh_token": "rt-1",
                    "client_id": "app_X",
                    "token_endpoint": format!("{}/token", mock.base),
                }),
            );

            let out = run_at(
                &format!("{}/backend", mock.base),
                &rt, "call-8", "codex", &conn, spec(), &params(), true,
            )
            .await
            .expect("driver run after refresh");
            assert_eq!(out["status"], 200);

            let reqs = mock.requests.lock().unwrap().clone();
            assert_eq!(reqs[0].path, "/backend");
            assert_eq!(reqs[0].header("authorization"), Some("Bearer at-old"));
            assert_eq!(reqs[1].path, "/token");
            assert!(reqs[1].body.contains("grant_type=refresh_token"));
            assert!(reqs[1].body.contains("client_id=app_X"));
            assert_eq!(reqs[2].path, "/putConnection", "the rotated grant must be stored");
            let retried = reqs.iter().find(|r| r.path == "/backend" && r.header("authorization") == Some("Bearer at-new"));
            assert!(retried.is_some(), "the retry must carry the refreshed token");
        }

        /// Upstream said no twice-over (a refreshless payload) — the error is
        /// the backend's own words plus the status, not a mystery timeout.
        #[tokio::test]
        async fn a_hard_4xx_surfaces_the_backends_words() {
            let mock = spawn_mock(vec![(403, "not your plan".into())]);
            let umk = Key::random();
            let conn = sealed_conn(&umk, json!({ "access_token": "at", "account_id": "a" }));
            let rt = Runtime::new(&mock.base, "s_tok", umk);
            let err = run_at(
                &format!("{}/backend", mock.base),
                &rt, "c", "codex", &conn, spec(), &params(), true,
            )
            .await
            .unwrap_err();
            assert!(err.contains("403") && err.contains("not your plan"), "{err}");
        }
    }
}
