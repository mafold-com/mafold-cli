//! Speaking MCP to a provider's own server.
//!
//! This is what turns a connection from a stored secret into a callable thing.
//! A provider that runs an MCP server already publishes its whole surface —
//! every tool, with a JSON Schema for its parameters — so Mafold does not need
//! a per-provider method table, and a tool the provider ships tomorrow appears
//! without a Mafold release. That is the entire reason this layer is MCP and
//! not a hand-written REST binding per provider.
//!
//! **Transport is Streamable HTTP** (MCP's HTTP binding): every request is a
//! JSON-RPC POST, and the reply is either `application/json` or an SSE stream
//! carrying the same JSON-RPC object. Both are parsed here because servers pick
//! freely — Notion answers SSE, and a server is allowed to change its mind per
//! request.
//!
//! ## Sessions, and why they are optional here
//!
//! The spec lets a server hand back an `Mcp-Session-Id` on `initialize` and
//! expect it on later requests. Native clients honour that. **Browsers cannot**:
//! a cross-origin response header is invisible unless the server lists it in
//! `Access-Control-Expose-Headers`, and neither Notion nor Figma lists it
//! (probed 2026-08-11 — Figma does not even *allow* the request header). So on
//! wasm the id is always absent, and this client is written to work without one
//! rather than to require something half its callers can never obtain. A server
//! that genuinely demands a session will fail the browser with its own words,
//! which is the honest outcome — not a silent one.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::net::{http_post, HttpReply};
use mafold_types::connections::AuthInfo;

/// The MCP revision this client implements. Sent on `initialize` and echoed as
/// a header afterwards; servers negotiate down if they must.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// One callable method, exactly as the provider describes it.
///
/// This is MCP's `Tool` under a Mafold name, and it stays MCP's shape on
/// purpose: `input_schema` is JSON Schema, which is what an agent's tool-use
/// loop already consumes. Translating it into a bespoke parameter language
/// would mean every consumer — the harness, an app, the cli's `--params` — had
/// to translate back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MethodSpec {
    pub name: String,
    /// Human title when the server offers one; falls back to `name`.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the arguments, verbatim.
    pub input_schema: Value,
    /// The server's own `readOnlyHint`. A *hint*, never a permission: it drives
    /// how a consent prompt is worded, and must not be the thing that decides
    /// whether a call is allowed.
    #[serde(default)]
    pub read_only: bool,
}

/// Why a call failed, split by what the caller should do about it.
#[derive(Debug, Clone)]
pub enum McpError {
    /// The credential was refused. The caller refreshes and retries **once** —
    /// this variant exists so that retry can't be triggered by an unrelated
    /// failure, which would turn one bad call into two.
    Unauthorized(String),
    /// Nothing usable came back: network, timeout, or a body that isn't
    /// JSON-RPC at all.
    Transport(String),
    /// The server answered, in protocol, with an error.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Unauthorized(m) => write!(f, "{m}"),
            McpError::Transport(m) => write!(f, "{m}"),
            McpError::Protocol(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for McpError {}

type Result<T> = std::result::Result<T, McpError>;

/// One conversation with one MCP server.
pub struct McpClient {
    url: String,
    /// Built once from the [`AuthInfo`] the caller resolved, so no code path
    /// anywhere decides where a provider's credential goes. `None` when there
    /// is no credential to send.
    auth: Option<(String, String)>,
    session_id: Option<String>,
    next_id: u64,
}

impl McpClient {
    /// `token` is the plaintext credential, already opened from the vault.
    ///
    /// An EMPTY token sends no credential header at all. A server that needs
    /// none (DeepWiki, a user's own open server) is a real shape now that a
    /// connection can name its own endpoint, and `Authorization: Bearer ` with
    /// nothing after it is not "no credential" to such a server — it is a
    /// malformed one, refused with a 401 that reads as a wrong token.
    pub fn new(url: &str, auth: &AuthInfo, token: &str) -> Self {
        Self {
            url: url.to_string(),
            auth: (!token.is_empty())
                .then(|| (auth.header.to_string(), format!("{}{}", auth.prefix, token))),
            session_id: None,
            next_id: 0,
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![
            ("content-type".into(), "application/json".into()),
            // Both are required by the Streamable HTTP binding: a server may
            // answer either way and refuses the request if we won't take both.
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
            ("mcp-protocol-version".into(), PROTOCOL_VERSION.into()),
        ];
        if let Some(auth) = &self.auth {
            h.push(auth.clone());
        }
        if let Some(s) = &self.session_id {
            h.push(("mcp-session-id".into(), s.clone()));
        }
        h
    }

    async fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params,
        });
        let reply = http_post(&self.url, &self.headers(), &body.to_string())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        // Capture a session id whenever one is offered. Absent on wasm — see
        // the module doc; that is a browser limit, not a server choice.
        if let Some(sid) = reply.header("mcp-session-id") {
            self.session_id = Some(sid.to_string());
        }
        Self::result_of(method, reply)
    }

    /// A notification: no id, so no reply to correlate. Failures here are not
    /// worth propagating — `notifications/initialized` is a courtesy, and a
    /// server that ignores it still answers `tools/list`.
    async fn notify(&mut self, method: &str) {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        let _ = http_post(&self.url, &self.headers(), &body.to_string()).await;
    }

    fn result_of(method: &str, reply: HttpReply) -> Result<Value> {
        if reply.status == 401 || reply.status == 403 {
            // The server's own sentence is far more useful than ours: this is
            // where Figma says "figd_ tokens must be passed via X-Figma-Token".
            let detail = reply.body.trim();
            let detail: String = detail.chars().take(200).collect();
            return Err(McpError::Unauthorized(if detail.is_empty() {
                format!("the provider rejected this credential (HTTP {})", reply.status)
            } else {
                detail
            }));
        }
        let envelope = parse_envelope(&reply.body).ok_or_else(|| {
            let snippet: String = reply.body.trim().chars().take(200).collect();
            McpError::Transport(if snippet.is_empty() {
                format!("{method}: HTTP {} with an empty body", reply.status)
            } else {
                format!("{method}: HTTP {} — {snippet}", reply.status)
            })
        })?;
        if let Some(err) = envelope.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the provider reported an error");
            return Err(McpError::Protocol(format!("{method}: {msg}")));
        }
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Handshake. Must precede everything else.
    pub async fn initialize(&mut self) -> Result<Value> {
        let info = self
            .send(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "mafold", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        self.notify("notifications/initialized").await;
        Ok(info)
    }

    /// Every tool the server offers, following `nextCursor` to the end.
    ///
    /// Paginating rather than taking page one matters: a catalog that silently
    /// stops at the first page looks complete, and the missing tools surface
    /// later as "the agent doesn't know how to do that" — indistinguishable
    /// from the provider not supporting it.
    pub async fn list_tools(&mut self) -> Result<Vec<MethodSpec>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // A server that keeps handing back a cursor would spin forever; stop
        // at a page count no real catalog reaches.
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.send("tools/list", params).await?;
            if let Some(tools) = page.get("tools").and_then(Value::as_array) {
                out.extend(tools.iter().map(tool_from_wire));
            }
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.send(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

/// MCP's wire `Tool` → our [`MethodSpec`].
///
/// Tolerant by design: `title`, `description` and `annotations` are all
/// optional in the spec, and a server that omits them should yield a usable
/// method rather than a parse failure that hides the whole catalog.
fn tool_from_wire(t: &Value) -> MethodSpec {
    let name = t.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    MethodSpec {
        title: t
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&name)
            .to_string(),
        description: t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_schema: t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
        read_only: t
            .get("annotations")
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        name,
    }
}

/// Pull the JSON-RPC object out of a body that may be plain JSON or SSE.
///
/// Split out and pure so both shapes are covered by tests without a server:
/// the SSE branch only runs against providers that choose it, so a bug there
/// would otherwise be found by a user rather than by CI.
fn parse_envelope(body: &str) -> Option<Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).ok();
    }
    // SSE: take the LAST `data:` payload that parses as a JSON-RPC object.
    // Last, not first, because a stream may carry progress notifications
    // ahead of the real result — taking the first would return a notification
    // as though it were the answer.
    let mut found = None;
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("data:") else { continue };
        let Ok(v) = serde_json::from_str::<Value>(rest.trim()) else { continue };
        if v.get("result").is_some() || v.get("error").is_some() {
            found = Some(v);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use mafold_types::connections::{BEARER, FIGMA_TOKEN_HEADER};

    #[test]
    fn plain_json_envelope() {
        let v = parse_envelope(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert!(v.get("result").is_some());
    }

    /// Notion answers SSE, so this is the live path for the provider we ship
    /// first — not a hypothetical.
    #[test]
    fn sse_envelope() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        assert!(parse_envelope(body).unwrap().get("result").is_some());
    }

    /// A progress notification arriving before the result must not be mistaken
    /// for the result.
    #[test]
    fn sse_skips_notifications_and_takes_the_result() {
        let body = concat!(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":1}}\n\n",
        );
        let v = parse_envelope(body).unwrap();
        assert_eq!(v["result"]["ok"], 1);
    }

    #[test]
    fn garbage_is_not_an_envelope() {
        assert!(parse_envelope("Unauthorized").is_none());
        assert!(parse_envelope("").is_none());
    }

    #[test]
    fn a_jsonrpc_error_becomes_protocol_not_transport() {
        let reply = HttpReply {
            status: 200,
            headers: vec![],
            body: r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown tool"}}"#
                .into(),
        };
        match McpClient::result_of("tools/call", reply) {
            Err(McpError::Protocol(m)) => assert!(m.contains("unknown tool"), "{m}"),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// 401 must be its own variant, because it is the only one that may be
    /// retried after refreshing a token.
    #[test]
    fn a_401_is_unauthorized_and_keeps_the_providers_words() {
        let reply = HttpReply {
            status: 401,
            headers: vec![],
            body: "figd_ tokens must be passed via X-Figma-Token header, not Authorization".into(),
        };
        match McpClient::result_of("initialize", reply) {
            Err(McpError::Unauthorized(m)) => assert!(m.contains("X-Figma-Token"), "{m}"),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    /// The point of `AuthInfo`: two providers, two header shapes, one client.
    #[test]
    fn the_credential_lands_where_the_registry_says() {
        let notion = McpClient::new("https://mcp.notion.com/mcp", &BEARER.into(), "ntn_abc");
        assert_eq!(notion.auth, Some(("Authorization".into(), "Bearer ntn_abc".into())));

        let figma = McpClient::new("https://mcp.figma.com/mcp", &FIGMA_TOKEN_HEADER.into(), "figd_xyz");
        assert_eq!(figma.auth, Some(("X-Figma-Token".into(), "figd_xyz".into())));
        // No prefix means no stray space — a header value of " figd_xyz" is
        // the kind of thing that fails as a plain 401 with nothing to read.
        assert!(!figma.auth.as_ref().unwrap().1.starts_with(' '));
    }

    /// A server that wants no credential must not be sent a malformed one.
    /// `Authorization: Bearer ` (empty) is refused by real servers as a bad
    /// token, which reads as "your credential is wrong" for a connection that
    /// has none — the self-described row made this a shape that exists.
    #[test]
    fn an_empty_credential_sends_no_header_at_all() {
        let open = McpClient::new("https://mcp.deepwiki.com/mcp", &BEARER.into(), "");
        assert_eq!(open.auth, None);
        assert!(
            !open.headers().iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
            "{:?}",
            open.headers()
        );
        // And the rest of the binding's headers are untouched by that choice.
        assert!(open.headers().iter().any(|(k, _)| k == "mcp-protocol-version"));
    }

    #[test]
    fn a_session_id_is_sent_once_known_and_omitted_before() {
        let mut c = McpClient::new("https://example.test/mcp", &BEARER.into(), "t");
        assert!(c.headers().iter().all(|(k, _)| k != "mcp-session-id"));
        c.session_id = Some("sess-1".into());
        assert_eq!(
            c.headers().iter().find(|(k, _)| k == "mcp-session-id").map(|(_, v)| v.as_str()),
            Some("sess-1")
        );
    }

    // ── over a real socket ──
    //
    // The pure tests above cover parsing; these cover everything between the
    // call and the wire — headers, JSON-RPC framing, the `initialized`
    // notification, pagination. That seam is where a client that "looks right"
    // still fails against a live server.

    #[cfg(not(target_arch = "wasm32"))]
    fn rpc_ok(result: &str) -> (u16, String) {
        (200, format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#))
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_full_handshake_then_catalog() {
        use crate::testutil::spawn_mock;
        let mock = spawn_mock(vec![
            rpc_ok(r#"{"protocolVersion":"2025-06-18","serverInfo":{"name":"stand-in"}}"#),
            // The `notifications/initialized` post — no id, no result expected.
            (202, "".into()),
            rpc_ok(
                r#"{"tools":[{"name":"search","description":"Find pages","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]},"annotations":{"readOnlyHint":true}}]}"#,
            ),
        ]);
        let mut c = McpClient::new(&format!("{}/mcp", mock.base), &BEARER.into(), "ntn_live");
        c.initialize().await.expect("initialize");
        let tools = c.list_tools().await.expect("tools/list");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        assert!(tools[0].read_only);
        assert_eq!(tools[0].input_schema["required"][0], "query");

        // The credential really went out, in the header the registry named.
        let init = mock.request(0);
        assert_eq!(init.header("authorization"), Some("Bearer ntn_live"));
        // Streamable HTTP: refusing either content type makes some servers 406.
        let accept = init.header("accept").unwrap_or("");
        assert!(accept.contains("application/json"), "{accept}");
        assert!(accept.contains("text/event-stream"), "{accept}");
        assert!(init.body.contains(r#""method":"initialize""#));

        // The handshake is completed, not just started.
        assert!(
            mock.request(1).body.contains("notifications/initialized"),
            "a server that waits for the initialized notification would hang here"
        );
        assert!(mock.request(2).body.contains(r#""method":"tools/list""#));
    }

    /// Figma's header, proven on the wire rather than in a struct field.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn figmas_credential_rides_its_own_header() {
        use crate::testutil::spawn_mock;
        let mock = spawn_mock(vec![rpc_ok("{}")]);
        let mut c = McpClient::new(
            &format!("{}/mcp", mock.base),
            &FIGMA_TOKEN_HEADER.into(),
            "figd_live",
        );
        c.initialize().await.expect("initialize");
        let req = mock.request(0);
        assert_eq!(req.header("x-figma-token"), Some("figd_live"));
        assert!(
            req.header("authorization").is_none(),
            "sending Authorization too is what Figma answers 401 to"
        );
    }

    /// A catalog that stops at page one looks complete and isn't — the missing
    /// tools resurface later as "the agent can't do that".
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn pagination_is_followed_to_the_end() {
        use crate::testutil::spawn_mock;
        let mock = spawn_mock(vec![
            rpc_ok("{}"),
            (202, "".into()),
            rpc_ok(r#"{"tools":[{"name":"a"}],"nextCursor":"p2"}"#),
            rpc_ok(r#"{"tools":[{"name":"b"}]}"#),
        ]);
        let mut c = McpClient::new(&format!("{}/mcp", mock.base), &BEARER.into(), "t");
        c.initialize().await.unwrap();
        let tools = c.list_tools().await.unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(mock.request(3).body.contains(r#""cursor":"p2""#));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_tool_call_sends_name_and_arguments() {
        use crate::testutil::spawn_mock;
        let mock = spawn_mock(vec![rpc_ok(
            r#"{"content":[{"type":"text","text":"3 results"}]}"#,
        )]);
        let mut c = McpClient::new(&format!("{}/mcp", mock.base), &BEARER.into(), "t");
        let out = c
            .call_tool("search", json!({ "query": "roadmap" }))
            .await
            .unwrap();
        assert_eq!(out["content"][0]["text"], "3 results");
        let sent: Value = serde_json::from_str(&mock.request(0).body).unwrap();
        assert_eq!(sent["method"], "tools/call");
        assert_eq!(sent["params"]["name"], "search");
        assert_eq!(sent["params"]["arguments"]["query"], "roadmap");
    }

    // ── live probes (`cargo test -- --ignored live_`) ──
    //
    // Ignored because they need the network, and because a provider being down
    // is not a reason for CI to be red. They exist so the claims this module is
    // built on stay checkable rather than becoming folklore: the registry says
    // Figma refuses `Authorization` and Notion accepts it, and that is exactly
    // the kind of assertion that silently stops being true.

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    #[ignore]
    async fn live_notion_takes_bearer_and_rejects_a_bad_one() {
        let mut c = McpClient::new("https://mcp.notion.com/mcp", &BEARER.into(), "ntn_not_a_real_token");
        match c.initialize().await {
            Err(McpError::Unauthorized(m)) => println!("notion said: {m}"),
            other => panic!("expected Unauthorized from a junk token, got {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    #[ignore]
    async fn live_figma_wants_its_own_header() {
        // Sent the registry's way: X-Figma-Token. A junk value must come back
        // as Unauthorized — NOT as the "must be passed via X-Figma-Token"
        // complaint, which would mean we put it in the wrong header.
        let mut c = McpClient::new(
            "https://mcp.figma.com/mcp",
            &FIGMA_TOKEN_HEADER.into(),
            "figd_not_a_real_token",
        );
        match c.initialize().await {
            Err(McpError::Unauthorized(m)) => {
                println!("figma said: {m}");
                assert!(
                    !m.contains("must be passed via"),
                    "we used the wrong header: {m}"
                );
            }
            other => panic!("expected Unauthorized from a junk token, got {other:?}"),
        }
    }

    /// The inverse, pinning WHY the registry carries an auth style at all: send
    /// Figma a bearer token and it names the header it actually wants.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    #[ignore]
    async fn live_figma_refuses_bearer() {
        let mut c = McpClient::new("https://mcp.figma.com/mcp", &BEARER.into(), "figd_not_a_real_token");
        match c.initialize().await {
            Err(McpError::Unauthorized(m)) => assert!(
                m.contains("X-Figma-Token"),
                "expected Figma to name its header, got: {m}"
            ),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_missing_its_optional_fields_still_parses() {
        let spec = tool_from_wire(&json!({ "name": "search" }));
        assert_eq!(spec.name, "search");
        // Title falls back to the name so a UI never renders a blank row.
        assert_eq!(spec.title, "search");
        assert_eq!(spec.input_schema, json!({ "type": "object" }));
        assert!(!spec.read_only);
    }

    #[test]
    fn annotations_drive_read_only() {
        let spec = tool_from_wire(&json!({
            "name": "fetch",
            "title": "Fetch a page",
            "description": "Read one Notion page",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } },
            "annotations": { "readOnlyHint": true }
        }));
        assert!(spec.read_only);
        assert_eq!(spec.title, "Fetch a page");
        assert_eq!(spec.input_schema["properties"]["id"]["type"], "string");
    }
}
