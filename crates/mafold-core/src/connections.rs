//! Using a connection — the part that turns a stored credential into calls.
//!
//! Everything here runs **on a device that holds the master key**, and that is
//! the whole architecture in one sentence. The server relays ciphertext it
//! cannot open, so the only place a credential can be assembled is a machine
//! its owner enrolled. Consequently this module lives in the core rather than
//! in any one client: the cli, the daemon's MCP aggregator, and the browser
//! (through wasm) all need it, and a second implementation would drift on
//! exactly the details — token refresh, payload filtering — where drift means a
//! credential quietly stops working.
//!
//! There is deliberately **no path here that asks the server to make a call**.
//! When no device is online, a call fails and says so. Routing a request to
//! whichever device happens to be live, or letting the server hold a delegated
//! credential, are both coherent designs — but they are different promises, and
//! they belong behind an explicit per-connection decision rather than as a
//! silent fallback. See `.docs/connections-v1.md`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::mcp::{McpClient, McpError, MethodSpec};
use crate::net;
use crate::vault::{self, Key};
use mafold_types::connections::ProviderInfo;

/// How long a fetched tool catalog stays fresh in memory.
///
/// Bounded rather than permanent because the aggregator is a long-lived
/// process: a provider that ships a new tool should become usable within a
/// coffee break, not at the next daemon restart. Short enough to notice, long
/// enough that a chatty agent never pays for `tools/list` twice in a turn.
const CATALOG_TTL_MS: i64 = 10 * 60 * 1000;

/// Renew this long before the token actually dies.
///
/// A token that expires *during* a call fails the call, and the retry costs a
/// round trip to a provider that has already said no. The margin also absorbs
/// clock skew between the device and the provider, which is otherwise invisible
/// and shows up as sporadic 401s on a credential that looks valid locally.
const RENEW_MARGIN_MS: i64 = 60 * 1000;

pub type Result<T> = std::result::Result<T, String>;

struct Cached {
    methods: Vec<MethodSpec>,
    at: i64,
}

/// A device's view of its owner's connections.
pub struct Runtime {
    base: String,
    /// The person's Mafold session token. Not a bot token: the api refuses
    /// those here, because a daemon being able to enumerate its owner's
    /// credentials just by running on their machine is the conflation this
    /// layer exists to prevent.
    token: String,
    umk: Key,
    catalogs: HashMap<String, Cached>,
}

impl Runtime {
    pub fn new(base: &str, token: &str, umk: Key) -> Self {
        Self {
            base: base.to_string(),
            token: token.to_string(),
            umk,
            catalogs: HashMap::new(),
        }
    }

    pub(crate) async fn rpc(&self, method: &str, body: Value) -> Result<Value> {
        let text = net::rpc(&self.base, &self.token, method, &body.to_string()).await?;
        serde_json::from_str(&text).map_err(|e| format!("{method}: {e}"))
    }

    /// Every connection the account holds, ciphertext included.
    pub async fn list(&self) -> Result<Vec<Value>> {
        let v = self.rpc("listConnections", serde_json::json!({})).await?;
        Ok(v.get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Value> {
        let items = self.list().await?;
        items
            .into_iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| format!("no connection named `{name}` — see `mafold connection list`"))
    }

    /// The descriptor behind a stored connection, from the served registry.
    ///
    /// Three outcomes, and they are three different sentences on purpose:
    /// no registry yet (a network problem, and temporary), a registry that has
    /// no such row (the provider was withdrawn — rare, and not the user's
    /// doing), or a row naming a native driver this build lacks (the only case
    /// left where "update the app" is the honest answer).
    ///
    /// It used to be a lookup in a compiled-in table, which made "this binary
    /// is older than your account" the answer to all three — including for a
    /// connection the user had just successfully linked.
    async fn descriptor_of(&self, conn: &Value) -> Result<ProviderInfo> {
        let id = conn.get("provider").and_then(Value::as_str).unwrap_or("");
        crate::providers::ensure(&self.base, &self.token, now_ms()).await?;
        crate::providers::get(id).ok_or_else(|| {
            format!(
                "the provider registry has no `{id}` — it may have been withdrawn; \
                 `mafold connection providers` lists what is current"
            )
        })
    }

    fn open(&self, conn: &Value) -> Result<Map<String, Value>> {
        let blob = conn.get("blob").and_then(Value::as_str).unwrap_or("");
        let dek = conn.get("wrapped_dek").and_then(Value::as_str).unwrap_or("");
        let plain = vault::open_payload(&self.umk, blob, dek).map_err(|e| e.to_string())?;
        serde_json::from_str(&plain).map_err(|e| format!("connection payload is not JSON: {e}"))
    }

    /// The MCP endpoint for a connection, or a sentence about why there is none.
    fn endpoint(spec: &ProviderInfo) -> Result<&str> {
        spec.mcp_url.as_deref().ok_or_else(|| {
            format!(
                "{} has no MCP server, so it has no methods — it is a credential to hand to a \
                 tool, not a thing to call",
                spec.display
            )
        })
    }

    /// Every method a connection offers, cached in memory.
    pub async fn methods(&mut self, name: &str) -> Result<Vec<MethodSpec>> {
        if let Some(c) = self.catalogs.get(name) {
            if now_ms() - c.at < CATALOG_TTL_MS {
                return Ok(c.methods.clone());
            }
        }
        let conn = self.get(name).await?;
        let spec = self.descriptor_of(&conn).await?;
        let url = Self::endpoint(&spec)?;
        let payload = self.refreshed_payload(name, &conn, &spec).await?;

        let methods = match self.catalog(url, &spec, &payload).await {
            Ok(m) => m,
            // The credential was refused even after any renewal above, so the
            // grant itself is gone (revoked at the provider, or a refresh token
            // that has been spent). Nothing to retry.
            Err(McpError::Unauthorized(m)) => {
                return Err(unauthorized_msg(name, &spec, &m));
            }
            Err(e) => return Err(e.to_string()),
        };
        self.catalogs.insert(
            name.to_string(),
            Cached {
                methods: methods.clone(),
                at: now_ms(),
            },
        );
        Ok(methods)
    }

    async fn catalog(
        &self,
        url: &str,
        spec: &ProviderInfo,
        payload: &Map<String, Value>,
    ) -> std::result::Result<Vec<MethodSpec>, McpError> {
        let mut client = McpClient::new(url, &spec.auth, &credential(spec, payload));
        client.initialize().await?;
        client.list_tools().await
    }

    /// Run one call on whatever surface the provider exposes: a native driver
    /// when the registry row names one, MCP otherwise. Streaming (chunked
    /// answers back to the api) is a native-driver affair — MCP answers stay
    /// single-shot.
    pub async fn call_any(
        &mut self,
        call_id: &str,
        name: &str,
        method: &str,
        params: &Value,
        stream: bool,
    ) -> Result<Value> {
        let conn = self.get(name).await?;
        let spec = self.descriptor_of(&conn).await?;
        // Reserved method: the tool CATALOG. The api's harness asks it (once
        // per connection, cached on both ends) to learn which tools a granted
        // connection offers. `tools/list` is MCP's own listing verb — a tool
        // name cannot contain `/`, so no provider can shadow it. Native-driver
        // providers (codex) expose no MCP tools; an empty list is the honest
        // answer, not an error.
        if method == "tools/list" {
            if spec.native_api.is_some() {
                return Ok(serde_json::json!({ "tools": [] }));
            }
            let methods = self.methods(name).await?;
            return Ok(serde_json::json!({
                "tools": methods
                    .iter()
                    .map(|m| serde_json::json!({
                        "name": m.name,
                        "title": m.title,
                        "description": m.description,
                        "inputSchema": m.input_schema,
                        "readOnly": m.read_only,
                    }))
                    .collect::<Vec<_>>()
            }));
        }
        match spec.native_api.as_deref() {
            Some("codex-responses") => match method {
                "responses" => {
                    crate::codex::run(self, call_id, name, &conn, &spec, params, stream).await
                }
                // One non-streaming POST to the images backend on the same
                // credential — the tool-call that asks for it is parsed
                // server-side, same split of knowledge as `responses`.
                "images.generate" => crate::codex::images(self, name, &conn, &spec, params).await,
                other => Err(format!(
                    "{} offers `responses` and `images.generate` — `{other}` is not one of them",
                    spec.display
                )),
            },
            // The ONE case where "update the app" is still the honest answer:
            // the pack can name a driver, but a driver is code. Everything else
            // a new provider needs now arrives with the pack.
            Some(other) => Err(format!(
                "`{name}` is driven by `{other}`, which this version doesn't know — update mafold"
            )),
            None => self.call(name, method, params.clone()).await,
        }
    }

    /// Run one MCP method.
    pub async fn call(&mut self, name: &str, method: &str, params: Value) -> Result<Value> {
        let conn = self.get(name).await?;
        let spec = self.descriptor_of(&conn).await?;
        let url = Self::endpoint(&spec)?;
        let payload = self.refreshed_payload(name, &conn, &spec).await?;

        let mut client = McpClient::new(url, &spec.auth, &credential(&spec, &payload));
        client.initialize().await.map_err(|e| match e {
            McpError::Unauthorized(m) => unauthorized_msg(name, &spec, &m),
            e => e.to_string(),
        })?;
        client
            .call_tool(method, params)
            .await
            .map_err(|e| match e {
                McpError::Unauthorized(m) => unauthorized_msg(name, &spec, &m),
                e => e.to_string(),
            })
    }

    /// The payload, renewed first if its token is at or near expiry.
    ///
    /// Renewal is **proactive**, driven by the stored `expires_at`, rather than
    /// reactive on a 401. Reacting to 401 alone would work, but it makes every
    /// first call after an idle hour pay a failed round trip, and it cannot
    /// distinguish "expired" from "revoked" — so it retries credentials that
    /// will never work again.
    pub(crate) async fn refreshed_payload(
        &self,
        name: &str,
        conn: &Value,
        spec: &ProviderInfo,
    ) -> Result<Map<String, Value>> {
        let payload = self.open(conn)?;
        if !needs_renewal(&payload) {
            return Ok(payload);
        }
        match self.renew(name, conn, spec, &payload).await {
            Ok(fresh) => Ok(fresh),
            // A failed renewal is not necessarily fatal: the old token may
            // still have seconds on it, and the provider is the authority on
            // that. Try the call; if it really is dead, the 401 path explains
            // what to do with words from the provider itself.
            Err(_) => Ok(payload),
        }
    }

    /// Spend the refresh token for a new access token, and store the result.
    ///
    /// Everything this needs travels inside the sealed payload — `client_id`
    /// and `token_endpoint` included — so a laptop daemon can renew a grant
    /// that a browser obtained. That is the entire reason those two are stored
    /// rather than treated as configuration: the browser registers its OAuth
    /// client dynamically, so the client id exists nowhere else in the world.
    pub(crate) async fn renew(
        &self,
        name: &str,
        conn: &Value,
        spec: &ProviderInfo,
        payload: &Map<String, Value>,
    ) -> Result<Map<String, Value>> {
        let get = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (refresh_token, client_id, endpoint) =
            (get("refresh_token"), get("client_id"), get("token_endpoint"));
        if refresh_token.is_empty() || client_id.is_empty() || endpoint.is_empty() {
            return Err("this connection has nothing to renew with".into());
        }

        let form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            form_encode(&refresh_token),
            form_encode(&client_id)
        );
        let reply = net::http_post(
            &endpoint,
            &[(
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            &form,
        )
        .await
        .map_err(|e| e.to_string())?;
        let grant: Value = serde_json::from_str(&reply.body)
            .map_err(|_| format!("the token endpoint answered HTTP {}", reply.status))?;
        let access = grant
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "the token endpoint returned no access token".to_string())?;

        let mut fresh = payload.clone();
        fresh.insert("access_token".into(), Value::String(access.to_string()));
        // Rotation is optional: a provider that returns no new refresh token
        // means the old one stays valid. Overwriting it with an empty string
        // would destroy the only way to ever renew again.
        if let Some(rt) = grant.get("refresh_token").and_then(Value::as_str) {
            if !rt.is_empty() {
                fresh.insert("refresh_token".into(), Value::String(rt.to_string()));
            }
        }
        if let Some(secs) = grant.get("expires_in").and_then(Value::as_i64) {
            fresh.insert(
                "expires_at".into(),
                Value::String((now_ms() + secs * 1000).to_string()),
            );
        }

        // Last writer wins. Two devices renewing at the same moment is
        // self-correcting rather than destructive: whichever grant the provider
        // honours is the one that gets stored, and the loser's next call
        // re-reads this row before doing anything with it.
        self.store(name, conn, spec, &fresh).await?;
        Ok(fresh)
    }

    /// Seal a payload and replace the stored connection.
    async fn store(
        &self,
        name: &str,
        conn: &Value,
        spec: &ProviderInfo,
        payload: &Map<String, Value>,
    ) -> Result<()> {
        let kept = filter_payload(spec, payload);
        let sealed = vault::seal_payload(&self.umk, &Value::Object(kept).to_string());
        self.rpc(
            "putConnection",
            serde_json::json!({
                "name": name,
                "provider": conn.get("provider").and_then(Value::as_str).unwrap_or(""),
                "label": conn.get("label").and_then(Value::as_str).unwrap_or(""),
                "blob": sealed.blob,
                "wrapped_dek": sealed.wrapped_dek,
                "key_id": conn.get("key_id").and_then(Value::as_str).unwrap_or(""),
            }),
        )
        .await?;
        Ok(())
    }
}

/// Answer a connection call that arrived on the socket, if that is what this
/// event is. Returns whether the event belonged to this handler.
///
/// This is what "a connection is core's automatic response" means concretely:
/// a call reaches a device the same way a message does — as an event on the
/// socket the client already holds — and the core answers it without any
/// separate process, port, or spawn. Every host feeds its unrecognised events
/// through here, so the daemon, the browser and the phone all answer
/// identically instead of three near-copies drifting apart.
///
/// The claim in the middle is not optional. The server fans the event out to
/// **every** live socket the account has, because presence is per-account; if
/// each of them just executed, three open devices would mean three searches, or
/// three pages created. Exactly one wins the claim and does the work.
pub async fn handle_event(rt: &mut Runtime, envelope: &str) -> bool {
    let Ok(env) = serde_json::from_str::<Value>(envelope) else {
        return false;
    };
    if env.get("method").and_then(Value::as_str) != Some("events.connectionCall") {
        return false;
    }
    let p = env.get("params").cloned().unwrap_or(Value::Null);
    let get = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (call_id, name, method) = (get("call_id"), get("connection"), get("method"));
    if call_id.is_empty() {
        return true;
    }

    // CAN this runtime actually execute the call? Answered BEFORE the claim,
    // because a claim is a promise: the server stops offering the call to
    // anybody else. A browser tab with an unlocked vault receives the same
    // fan-out a daemon does and used to win this race — and then discover that
    // a native driver's cross-origin POST (chatgpt.com) is structurally
    // blocked by CORS, answering the caller with
    // "transport failed: TypeError: Failed to fetch" while the daemon that
    // could have served it sat one claim behind (owner hit exactly this on a
    // resale turn, 2026-08-19). MCP calls stay claimable everywhere: their
    // endpoints speak CORS, which is why the browser is on this bus at all.
    // Declining is SILENT — the device that can run it claims instead, and if
    // none exists the server's claim timeout names the real situation.
    if cfg!(target_arch = "wasm32") {
        // FAIL CLOSED. The first cut resolved the spec and claimed on
        // `native_api == None` — but `descriptor_of` needs the served provider
        // registry, and a freshly opened tab hasn't cached it yet: the resolve
        // errored, `.ok()` read as "not native", and the browser claimed a
        // codex call through the very hole this gate exists to close (owner
        // reproduced it on a live resale turn within minutes of the fix
        // shipping). A device that cannot PROVE it can run the call must not
        // promise to — declining is free, the daemon is one claim behind.
        let can = match rt.get(&name).await {
            Ok(conn) => match rt.descriptor_of(&conn).await {
                Ok(spec) => spec.native_api.is_none(),
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !can {
            return true;
        }
    }

    // Ask before working. A device that loses the claim is done — silently, and
    // without having touched the provider.
    let claimed = rt
        .rpc("claimConnectionCall", serde_json::json!({ "call_id": call_id }))
        .await
        .ok()
        .and_then(|v| v.get("claimed").and_then(Value::as_bool))
        .unwrap_or(false);
    if !claimed {
        return true;
    }

    let params = p.get("params").cloned().unwrap_or(serde_json::json!({}));
    let stream = p.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let answer = match rt.call_any(&call_id, &name, &method, &params, stream).await {
        Ok(result) => serde_json::json!({ "call_id": call_id, "result": result }),
        Err(e) => serde_json::json!({ "call_id": call_id, "error": e }),
    };
    // Always answer, including on failure: the caller is parked on this, and a
    // silent drop turns a nameable error ("Notion refused this credential")
    // into a 30-second timeout that says nothing.
    let _ = rt.rpc("answerConnectionCall", answer).await;
    true
}

/// Keep exactly the keys the registry declares for this provider.
///
/// Filtered against every declared key — including the ones no form ever draws.
/// Filtering against the *renderable* fields is what silently dropped
/// `expires_at` from every Notion grant, turning a refreshable connection into
/// one that died after an hour with nothing recorded about why.
pub fn filter_payload(spec: &ProviderInfo, payload: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for key in &spec.payload_keys {
        match payload.get(key) {
            Some(Value::String(s)) if !s.is_empty() => {
                out.insert(key.clone(), Value::String(s.clone()));
            }
            // Providers are inconsistent about number-vs-string for expiry.
            Some(Value::Number(n)) => {
                out.insert(key.clone(), Value::String(n.to_string()));
            }
            _ => {}
        }
    }
    out
}

/// The credential itself, from wherever the registry says it lives.
fn credential(spec: &ProviderInfo, payload: &Map<String, Value>) -> String {
    payload
        .get(&spec.auth.field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Whether a payload's token is close enough to expiry to renew now.
///
/// No `expires_at` means "don't know", and don't-know must mean **don't
/// renew** — Figma's personal access tokens carry no expiry at all, and
/// treating an absent field as expired would spend a refresh token that
/// doesn't exist on every single call.
pub fn needs_renewal(payload: &Map<String, Value>) -> bool {
    let Some(at) = payload.get("expires_at") else {
        return false;
    };
    let ms = match at {
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    };
    match ms {
        Some(ms) => now_ms() + RENEW_MARGIN_MS >= ms,
        // Unparseable is also don't-know.
        None => false,
    }
}

/// What to tell a person whose credential was refused.
///
/// Keeps the provider's own sentence — it is usually the specific one — and
/// adds the only thing the provider cannot know: which Mafold connection this
/// was, and how to fix it here.
fn unauthorized_msg(name: &str, spec: &ProviderInfo, detail: &str) -> String {
    format!(
        "{} refused this credential: {detail}\n  \
         re-link it:  mafold connection add {name} --provider {}",
        spec.display, spec.id
    )
}

/// Percent-encode one `application/x-www-form-urlencoded` value.
///
/// Hand-rolled to keep a dependency out of a crate that compiles to wasm. Only
/// unreserved characters survive unescaped, which is stricter than required and
/// therefore safe for tokens whose alphabet we don't control.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mafold_types::connections::provider_infos;
    use serde_json::json;

    /// One descriptor, in the SERVED shape.
    ///
    /// Tests read the same `provider_infos()` a publish serialises, so they
    /// exercise what a client actually receives rather than the authoring const
    /// it is derived from.
    fn info(id: &str) -> ProviderInfo {
        provider_infos().into_iter().find(|p| p.id == id).expect("no such provider")
    }

    /// Put a registry in this process, as a fetch would. `now_ms()` keeps it
    /// inside the freshness window, so nothing here touches the network.
    fn with_registry() {
        crate::providers::install_unverified_for_tests(1, provider_infos(), now_ms());
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_payload_with_no_expiry_is_never_renewed() {
        // Figma's PAT. Renewing it would spend a refresh token it doesn't have.
        assert!(!needs_renewal(&map(json!({ "access_token": "figd_x" }))));
    }

    #[test]
    fn expiry_is_read_as_either_string_or_number() {
        let past = (now_ms() - 1000).to_string();
        assert!(needs_renewal(&map(json!({ "expires_at": past }))));
        assert!(needs_renewal(&map(json!({ "expires_at": now_ms() - 1000 }))));
    }

    #[test]
    fn a_token_inside_the_margin_renews_early() {
        let soon = now_ms() + RENEW_MARGIN_MS / 2;
        assert!(
            needs_renewal(&map(json!({ "expires_at": soon.to_string() }))),
            "a token expiring inside the margin must renew before the call, not after it fails"
        );
        let later = now_ms() + RENEW_MARGIN_MS * 10;
        assert!(!needs_renewal(&map(json!({ "expires_at": later.to_string() }))));
    }

    #[test]
    fn an_unparseable_expiry_does_not_trigger_renewal() {
        assert!(!needs_renewal(&map(json!({ "expires_at": "soon" }))));
    }

    /// The regression this whole field split exists for.
    #[test]
    fn filtering_keeps_the_fields_a_form_never_draws() {
        let notion = info("notion");
        let kept = filter_payload(
            &notion,
            &map(json!({
                "access_token": "ntn_a",
                "refresh_token": "ntn_r",
                "expires_at": "1760000000000",
                "client_id": "dyn-123",
                "token_endpoint": "https://mcp.notion.com/token",
                "workspace_name": "Acme"
            })),
        );
        assert_eq!(kept.get("expires_at").unwrap(), "1760000000000");
        assert_eq!(kept.get("client_id").unwrap(), "dyn-123");
        assert_eq!(kept.get("token_endpoint").unwrap(), "https://mcp.notion.com/token");
        // Anything the registry doesn't declare still stays out — the filter is
        // a whitelist, not a passthrough.
        assert!(kept.get("workspace_name").is_none());
    }

    #[test]
    fn a_numeric_expiry_is_normalised_to_a_string() {
        let kept = filter_payload(
            &info("notion"),
            &map(json!({ "access_token": "t", "expires_at": 1760000000000i64 })),
        );
        assert_eq!(kept.get("expires_at").unwrap(), "1760000000000");
    }

    /// Each provider's credential is read from the field its registry row
    /// names — not from a key this module assumes.
    #[test]
    fn the_credential_comes_from_the_registrys_field() {
        assert_eq!(
            credential(
                &info("notion"),
                &map(json!({ "access_token": "ntn_a" }))
            ),
            "ntn_a"
        );
        assert_eq!(
            credential(
                &info("anthropic-api"),
                &map(json!({ "api_key": "sk-ant-1" }))
            ),
            "sk-ant-1"
        );
    }

    /// A provider with no MCP server is not a broken connection, and the
    /// message has to say which it is.
    #[test]
    fn a_provider_without_mcp_explains_itself() {
        let err = Runtime::endpoint(&info("anthropic-api")).unwrap_err();
        assert!(err.contains("no MCP server"), "{err}");
        assert!(Runtime::endpoint(&info("notion")).is_ok());
        assert!(Runtime::endpoint(&info("figma")).is_ok());
    }

    /// A row the registry doesn't have is no longer "your binary is old".
    ///
    /// It used to be, and that was the bug: a provider added on Tuesday made
    /// every client say "update mafold" until five apps had shipped, including
    /// to the user who had just linked it successfully from their browser.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_provider_missing_from_the_pack_is_not_a_version_problem() {
        with_registry();
        // `base` is never reached: the pack was installed at `now_ms()`, so
        // `ensure` is inside its freshness window and does no I/O. That is the
        // property that keeps a cached registry working offline.
        let rt = Runtime::new("http://127.0.0.1:1", "s_tok", Key::random());
        let err = rt.descriptor_of(&json!({ "provider": "dropbox" })).await.unwrap_err();
        assert!(err.contains("no `dropbox`"), "{err}");
        assert!(!err.contains("update mafold"), "not a version problem: {err}");

        // And one the pack DOES carry resolves, with the routing a call needs.
        let figma = rt.descriptor_of(&json!({ "provider": "figma" })).await.unwrap();
        assert_eq!(figma.auth.header, "X-Figma-Token");
    }

    /// With no pack at all the sentence has to be about the registry, not about
    /// the connection — and it must not be silently treated as "no providers".
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn no_registry_yet_says_so_rather_than_blaming_the_connection() {
        crate::providers::forget();
        let rt = Runtime::new("http://127.0.0.1:1", "s_tok", Key::random());
        let err = rt.descriptor_of(&json!({ "provider": "notion" })).await.unwrap_err();
        assert!(err.contains("no provider registry yet"), "{err}");
        with_registry();
    }

    #[test]
    fn form_encoding_escapes_what_a_token_might_contain() {
        assert_eq!(form_encode("abc-_.~123"), "abc-_.~123");
        assert_eq!(form_encode("a+b/c=d&e"), "a%2Bb%2Fc%3Dd%26e");
    }

    // ── the socket handler ──

    #[cfg(not(target_arch = "wasm32"))]
    fn rt_against(base: &str) -> Runtime {
        Runtime::new(base, "s_token", Key::random())
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn an_unrelated_event_is_not_ours_and_costs_nothing() {
        use crate::testutil::spawn_mock;
        let mock = spawn_mock(vec![crate::testutil::ok("null")]);
        let mut rt = rt_against(&mock.base);
        let handled = handle_event(
            &mut rt,
            r#"{"method":"events.messageNew","params":{"id":"1"}}"#,
        )
        .await;
        assert!(!handled, "a message must fall through to the host's own dispatch");
        assert!(
            mock.requests.lock().unwrap().is_empty(),
            "an unrelated event must not touch the network"
        );
    }

    /// The guard the whole claim step exists for: three devices online means
    /// three of these run, and the two that lose must do NOTHING — no provider
    /// traffic, no answer racing the winner's.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn losing_the_claim_ends_the_work_immediately() {
        use crate::testutil::{ok, spawn_mock};
        let mock = spawn_mock(vec![ok(r#"{"claimed":false}"#)]);
        let mut rt = rt_against(&mock.base);
        let handled = handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-1","connection":"notion","method":"search","params":{}}}"#,
        )
        .await;

        assert!(handled, "it was ours, we just didn't win it");
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(
            reqs.len(),
            1,
            "a losing device must stop after the claim, not answer as well"
        );
        assert_eq!(reqs[0].path, "/claimConnectionCall");
        assert!(reqs[0].body.contains("c-1"));
    }

    /// A malformed call is still ours to swallow — handing it back to the host
    /// would only produce a second "unknown event" complaint about an event
    /// that is very much known.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_call_with_no_id_is_consumed_without_a_round_trip() {
        use crate::testutil::{ok, spawn_mock};
        let mock = spawn_mock(vec![ok("null")]);
        let mut rt = rt_against(&mock.base);
        assert!(
            handle_event(
                &mut rt,
                r#"{"method":"events.connectionCall","params":{"connection":"notion"}}"#
            )
            .await
        );
        assert!(mock.requests.lock().unwrap().is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn garbage_on_the_socket_is_not_ours() {
        let mut rt = rt_against("http://127.0.0.1:1");
        assert!(!handle_event(&mut rt, "not json at all").await);
    }

    #[test]
    fn the_unauthorized_message_keeps_the_providers_own_words() {
        let m = unauthorized_msg(
            "design",
            &info("figma"),
            "figd_ tokens must be passed via X-Figma-Token header",
        );
        assert!(m.contains("X-Figma-Token"), "{m}");
        assert!(m.contains("mafold connection add design --provider figma"), "{m}");
    }
}
