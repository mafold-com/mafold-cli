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
use mafold_types::connections::{AuthInfo, ProviderInfo};

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

/// What a host installs to prove it can run a `computer` connection.
///
/// Two halves, and both are needed before this device may claim a call:
/// **who it is** (the device id it registered with the vault — the same string
/// the connection's sealed payload names) and **how it runs things**. A host
/// that knows its id but cannot spawn a process, or one that can spawn but is
/// not the machine the connection points at, must decline — see
/// [`Runtime::can_serve`].
pub struct ComputerHost {
    /// This machine's vault device id.
    pub device_id: String,
    /// Runs one already-validated [`Job`](crate::computer::Job). Installed by
    /// the host rather than implemented here because four of the five surfaces
    /// that link this core have no business spawning a shell.
    pub run: Executor,
}

/// See [`ComputerHost::run`].
pub type Executor = std::sync::Arc<
    dyn Fn(crate::computer::Job) -> futures::future::BoxFuture<'static, Result<Value>>
        + Send
        + Sync,
>;

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
    /// Set only on a host that can actually run a shell — see [`ComputerHost`].
    computer: Option<ComputerHost>,
}

impl Runtime {
    pub fn new(base: &str, token: &str, umk: Key) -> Self {
        Self {
            base: base.to_string(),
            token: token.to_string(),
            umk,
            catalogs: HashMap::new(),
            computer: None,
        }
    }

    /// Declare that this process IS a machine the user can call, and how to run
    /// things on it.
    ///
    /// Opt-in, and never a default: a `Runtime` built by the web's wasm or by
    /// an iOS app leaves this `None` and therefore answers no computer call —
    /// which is the correct answer, not a limitation. The daemon calls it once
    /// at startup with its own vault device id.
    pub fn attach_computer(&mut self, device_id: impl Into<String>, run: Executor) {
        self.computer = Some(ComputerHost {
            device_id: device_id.into(),
            run,
        });
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
    ///
    /// **The registry speaks first, and the payload only when it is silent.**
    /// A row that names an `mcp_url` is sent there and nowhere else — a sealed
    /// payload cannot redirect Notion's token, however it was written. A row
    /// that instead delegates ([`ProviderInfo::delegates_endpoint`]) is the
    /// user's own server, and the address lives in the ciphertext because that
    /// is the one place the api provably cannot put an address of its choosing
    /// (`.docs/custom-mcp-v1.md` §2). Every other row has no server at all.
    ///
    /// Takes the OPENED payload, which is why callers open before they ask:
    /// the order used to be the reverse, and for a delegating row that is a
    /// question asked before the answer exists.
    fn endpoint(spec: &ProviderInfo, payload: &Map<String, Value>) -> Result<String> {
        if let Some(url) = spec.mcp_url.as_deref() {
            return Ok(url.to_string());
        }
        if spec.delegates_endpoint() {
            return payload
                .get("endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    // A fixable state, and the sentence must say so: this is a
                    // row that was sealed without its address, not a provider
                    // that has none.
                    "this connection carries no server address — link it again from \
                     Settings ▸ Connections"
                        .to_string()
                });
        }
        Err(format!(
            "{} has no MCP server, so it has no methods — it is a credential to hand to a \
             tool, not a thing to call",
            spec.display
        ))
    }

    /// How the credential rides for this connection.
    ///
    /// The registry's [`AuthInfo`] for every row that names its server. For a
    /// delegating row the payload may carry `auth_header` / `auth_prefix` —
    /// the link flow writes them for a server that wants something other than
    /// `Authorization: Bearer` (Figma's `X-Figma-Token` is the shape this is
    /// for). The FIELD never moves: it is `access_token` on both sides so the
    /// common case needs no override at all.
    fn auth_for(spec: &ProviderInfo, payload: &Map<String, Value>) -> AuthInfo {
        let mut auth = spec.auth.clone();
        if !spec.delegates_endpoint() {
            return auth;
        }
        // The prefix is NOT trimmed: its trailing space is the whole point
        // (`Bearer ` vs `Bearer`), and a server reads `BearerTOKEN` as garbage.
        let raw = |k: &str| {
            payload
                .get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        };
        if let Some(header) = raw("auth_header") {
            auth.header = header.trim().to_string();
            // A custom header carries the token verbatim unless the payload
            // says otherwise: `X-Api-Key: Bearer …` is what nobody means.
            auth.prefix = raw("auth_prefix").unwrap_or_default();
        } else if let Some(prefix) = raw("auth_prefix") {
            auth.prefix = prefix;
        }
        auth
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
        // A machine's verbs are ours, not a server's — no endpoint to ask and
        // nothing to cache. Falling through would reach `endpoint()` and answer
        // "it has no MCP server, so it has no methods", which is precisely
        // wrong for the one provider whose methods this build implements.
        if spec.native_api.as_deref() == Some(crate::computer::DRIVER) {
            return Ok(crate::computer::method_specs());
        }
        // Open before asking where to send: a delegating row keeps its address
        // in the payload.
        let payload = self.refreshed_payload(name, &conn, &spec).await?;
        let url = Self::endpoint(&spec, &payload)?;

        let methods = match self.catalog(&url, &spec, &payload).await {
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
        let mut client =
            McpClient::new(url, &Self::auth_for(spec, payload), &credential(spec, payload));
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
            // A native driver's catalog is the DRIVER's business, not a blanket
            // empty list. Codex has no tools — `responses` is a model call, and
            // showing it to an agent as one would be inviting it to nest a
            // model inside a turn. A computer's shell verbs, by contrast, are
            // exactly tools, and an agent granted a machine has to be able to
            // discover them the same way it discovers Notion's.
            if let Some(driver) = spec.native_api.as_deref() {
                return Ok(if driver == crate::computer::DRIVER {
                    crate::computer::catalog()
                } else {
                    serde_json::json!({ "tools": [] })
                });
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
            Some(d) if d == crate::computer::DRIVER => {
                self.run_computer(name, &conn, method, params).await
            }
            // The ONE case where "update the app" is still the honest answer:
            // the pack can name a driver, but a driver is code. Everything else
            // a new provider needs now arrives with the pack.
            Some(other) => Err(format!(
                "`{name}` is driven by `{other}`, which this version doesn't know — update mafold"
            )),
            None => self.call(name, method, params.clone()).await,
        }
    }

    /// Run one method on a machine — if this process IS that machine.
    ///
    /// The binding lives in the sealed payload, so this check is the one thing
    /// in the whole path the server cannot influence: it relays a call to an
    /// ACCOUNT, and the ciphertext decides which of that account's machines was
    /// meant. Getting it wrong is not a permission bug, it is running someone's
    /// build command on their other laptop.
    async fn run_computer(
        &self,
        name: &str,
        conn: &Value,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        let payload = self.open(conn)?;
        let str_at = |k: &str| {
            payload
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (bound, machine) = (str_at("device_id"), str_at("machine"));
        if bound.is_empty() {
            return Err(format!(
                "`{name}` names no machine — it was written by something that didn't finish \
                 binding. Re-add it on the computer it should point at"
            ));
        }
        let machine = if machine.is_empty() {
            "another machine".to_string()
        } else {
            machine
        };
        let host = self.computer.as_ref().ok_or_else(|| {
            format!("`{name}` runs on {machine}; this surface cannot run shell commands")
        })?;
        if host.device_id != bound {
            return Err(format!(
                "`{name}` is {machine}, and this isn't it. That machine answers when it's \
                 online — `mafold up` there"
            ));
        }
        let job = crate::computer::parse(method, params)?;
        // Cloned so the executor's future borrows nothing of `self`: a shell
        // command outlives this frame by design.
        let run = host.run.clone();
        run(job).await
    }

    /// CAN this runtime execute a call for `name`? Asked BEFORE the claim,
    /// because a claim is a promise: the server stops offering the call to
    /// anybody else, so a device that answers "I'll take it" and then discovers
    /// it cannot is worse than one that never spoke.
    ///
    /// Two ways to be unable, and both were learned the hard way:
    ///
    ///  * a **browser** cannot POST a native driver's endpoint (chatgpt.com is
    ///    structurally CORS-blocked), and used to win this race against the
    ///    daemon that could have served it — owner hit exactly that on a live
    ///    resale turn, 2026-08-19;
    ///  * a device that is **not the machine** a computer connection names can
    ///    open the payload perfectly well and still must not run the command.
    ///
    /// Declining is SILENT. The device that can run it claims instead, and if
    /// none exists the caller's timeout names the real situation.
    ///
    /// Public because a CALLER wants the same answer for the opposite reason:
    /// `mafold connection call` asks it to decide between running the thing
    /// here and asking the server to carry the request to the machine that can.
    pub async fn can_serve(&mut self, name: &str, method: &str) -> bool {
        // Resolving costs a round trip, and paying it before every claim would
        // slow the path that three online devices are already racing on. A
        // device that can run processes only needs to look when the method is
        // one a MACHINE answers; the browser looks always, because for it the
        // question is about the transport rather than the machine.
        if !cfg!(target_arch = "wasm32") && !crate::computer::owns_method(method) {
            return true;
        }
        let resolved = match self.get(name).await {
            Ok(conn) => self.descriptor_of(&conn).await.ok().map(|spec| (conn, spec)),
            Err(_) => None,
        };
        let Some((conn, spec)) = resolved else {
            // FAIL CLOSED in the browser. The first cut of this gate resolved
            // the spec and claimed on `native_api == None` — but a freshly
            // opened tab has no provider registry cached yet, the resolve
            // errored, `.ok()` read as "not native", and the browser claimed a
            // codex call through the very hole the gate exists to close. A
            // device holding the vault claims anyway: it can report the real
            // error, which a caller needs more than a silent timeout.
            return !cfg!(target_arch = "wasm32");
        };
        match spec.native_api.as_deref() {
            Some(d) if d == crate::computer::DRIVER => {
                let bound = self
                    .open(&conn)
                    .ok()
                    .and_then(|p| p.get("device_id").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                !bound.is_empty()
                    && self
                        .computer
                        .as_ref()
                        .is_some_and(|host| host.device_id == bound)
            }
            Some(_) => !cfg!(target_arch = "wasm32"),
            None => true,
        }
    }

    /// Run one MCP method.
    pub async fn call(&mut self, name: &str, method: &str, params: Value) -> Result<Value> {
        let conn = self.get(name).await?;
        let spec = self.descriptor_of(&conn).await?;
        let payload = self.refreshed_payload(name, &conn, &spec).await?;
        let url = Self::endpoint(&spec, &payload)?;

        let mut client =
            McpClient::new(&url, &Self::auth_for(&spec, &payload), &credential(&spec, &payload));
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

    // CAN this runtime actually execute the call? Answered BEFORE the claim —
    // see `Runtime::can_serve`, which is where the two ways to be unable (a
    // browser facing CORS, a machine that isn't the one named) are spelled out.
    // A device that cannot PROVE it can run the call must not promise to.
    if !rt.can_serve(&name, &method).await {
        return true;
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
    if spec.delegates_endpoint() {
        // The server is the user's own choice, so "which provider" says
        // nothing useful; where to re-link does. The address it needs is in
        // the payload the caller just opened — not repeated here, where it
        // would ride into logs alongside the provider's refusal.
        return format!(
            "the server behind `{name}` refused this credential: {detail}\n  \
             re-link it from Settings ▸ Connections, or:  \
             mafold connection add {name} --provider {} --url <server>",
            spec.id
        );
    }
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

    /// Put a registry in this process, as a fetch would, and HOLD it there for
    /// the rest of the test. `now_ms()` keeps it inside the freshness window,
    /// so nothing here touches the network.
    #[must_use = "bind the guard (`let _reg = with_registry();`) — dropped immediately, \
                  another test can empty the registry mid-call"]
    fn with_registry() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::providers::test_lock();
        crate::providers::install_unverified_for_tests(1, provider_infos(), now_ms());
        guard
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
        let none = Map::new();
        let err = Runtime::endpoint(&info("anthropic-api"), &none).unwrap_err();
        assert!(err.contains("no MCP server"), "{err}");
        assert!(Runtime::endpoint(&info("notion"), &none).is_ok());
        assert!(Runtime::endpoint(&info("figma"), &none).is_ok());
    }

    /// The self-described row: its server is wherever the SEALED payload says.
    #[test]
    fn a_delegating_row_reads_its_server_from_the_payload() {
        let payload = map(json!({ "endpoint": " https://mcp.stripe.com/ ", "access_token": "t" }));
        assert_eq!(
            Runtime::endpoint(&info("mcp"), &payload).unwrap(),
            "https://mcp.stripe.com/"
        );
    }

    /// **The security property of the whole design.** A row that names its
    /// server is sent there and nowhere else — an `endpoint` smuggled into
    /// Notion's payload changes nothing. Without this, the sealed payload would
    /// be a second place to redirect a credential, and the registry signature
    /// would guard only half the door.
    #[test]
    fn a_registry_row_ignores_an_endpoint_in_its_payload() {
        let hostile = map(json!({ "endpoint": "https://evil.example/mcp", "access_token": "t" }));
        assert_eq!(
            Runtime::endpoint(&info("notion"), &hostile).unwrap(),
            info("notion").mcp_url.unwrap()
        );
        // And a row with no server stays a row with no server: `anthropic-api`
        // did not delegate, so an `endpoint` in its payload is just a stray key.
        let err = Runtime::endpoint(&info("anthropic-api"), &hostile).unwrap_err();
        assert!(err.contains("no MCP server"), "{err}");
    }

    /// A delegating row sealed without its address is a FIXABLE state, and the
    /// sentence must send the person to the fix rather than call the provider
    /// server-less.
    #[test]
    fn a_delegating_row_without_an_address_says_to_relink() {
        for payload in [json!({}), json!({ "endpoint": "  " })] {
            let err = Runtime::endpoint(&info("mcp"), &map(payload)).unwrap_err();
            assert!(err.contains("link it again"), "{err}");
            assert!(!err.contains("no MCP server"), "{err}");
        }
    }

    /// The credential's header comes from the payload only for a delegating
    /// row, and the FIELD never moves.
    #[test]
    fn payload_auth_overrides_apply_only_to_a_delegating_row() {
        // Default ride: standard bearer, nothing overridden.
        let plain = Runtime::auth_for(&info("mcp"), &map(json!({ "endpoint": "https://x" })));
        assert_eq!((plain.header.as_str(), plain.prefix.as_str(), plain.field.as_str()),
                   ("Authorization", "Bearer ", "access_token"));

        // A custom header carries the token verbatim unless a prefix is given.
        let keyed = Runtime::auth_for(
            &info("mcp"),
            &map(json!({ "endpoint": "https://x", "auth_header": "X-Api-Key" })),
        );
        assert_eq!((keyed.header.as_str(), keyed.prefix.as_str()), ("X-Api-Key", ""));
        let both = Runtime::auth_for(
            &info("mcp"),
            &map(json!({ "endpoint": "https://x", "auth_header": "X-Token", "auth_prefix": "Token " })),
        );
        assert_eq!((both.header.as_str(), both.prefix.as_str()), ("X-Token", "Token "));
        // Prefix alone keeps the standard header.
        let prefixed = Runtime::auth_for(
            &info("mcp"),
            &map(json!({ "endpoint": "https://x", "auth_prefix": "Basic " })),
        );
        assert_eq!((prefixed.header.as_str(), prefixed.prefix.as_str()), ("Authorization", "Basic "));
        assert_eq!(prefixed.field, "access_token");

        // Notion's payload cannot change how Notion's credential rides.
        let notion = Runtime::auth_for(
            &info("notion"),
            &map(json!({ "auth_header": "X-Evil", "auth_prefix": "" })),
        );
        assert_eq!(notion, info("notion").auth);
    }

    /// The address and the auth override are payload keys the row declares,
    /// so sealing keeps them — the way `expires_at` was NOT kept, once.
    #[test]
    fn sealing_a_delegating_row_keeps_its_address_and_ride() {
        let kept = filter_payload(
            &info("mcp"),
            &map(json!({
                "endpoint": "https://mcp.stripe.com/",
                "access_token": "t",
                "auth_header": "X-Api-Key",
                "stray": "dropped",
            })),
        );
        assert_eq!(kept.get("endpoint").unwrap(), "https://mcp.stripe.com/");
        assert_eq!(kept.get("auth_header").unwrap(), "X-Api-Key");
        assert!(kept.get("stray").is_none());
    }

    /// The refusal message for a delegating row names the fix, not a vendor —
    /// there is no vendor — and does not repeat the address.
    #[test]
    fn a_delegating_rows_refusal_points_at_settings() {
        let m = unauthorized_msg("stripe", &info("mcp"), "invalid_token");
        assert!(m.contains("Settings ▸ Connections"), "{m}");
        assert!(m.contains("--provider mcp --url"), "{m}");
        assert!(m.contains("invalid_token"), "{m}");
    }

    /// A row the registry doesn't have is no longer "your binary is old".
    ///
    /// It used to be, and that was the bug: a provider added on Tuesday made
    /// every client say "update mafold" until five apps had shipped, including
    /// to the user who had just linked it successfully from their browser.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_provider_missing_from_the_pack_is_not_a_version_problem() {
        let _reg = with_registry();
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
        // Held for the whole test: emptying a process-global out from under a
        // sibling test is how this file got a failure that pointed at Notion.
        let _reg = crate::providers::test_lock();
        crate::providers::forget();
        let rt = Runtime::new("http://127.0.0.1:1", "s_tok", Key::random());
        let err = rt.descriptor_of(&json!({ "provider": "notion" })).await.unwrap_err();
        assert!(err.contains("no provider registry yet"), "{err}");
        // Put it back before releasing the lock — NOT via `with_registry()`,
        // which would try to take a lock this thread already holds.
        crate::providers::install_unverified_for_tests(1, provider_infos(), now_ms());
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

    // ── a server of your own naming ──

    /// What a server the person named looks like to this process: an HTTP
    /// endpoint that speaks JSON-RPC and has never heard of Mafold. Records
    /// the headers of every request so a test can say what was — and was NOT
    /// — sent with them.
    #[cfg(not(target_arch = "wasm32"))]
    async fn fake_mcp_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<HashMap<String, String>>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (head, body) = req.split_once("\r\n\r\n").unwrap_or((&req, ""));
                    let headers: HashMap<String, String> = head
                        .lines()
                        .skip(1)
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        .collect();
                    sink.lock().unwrap().push(headers);
                    let rpc: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                    let id = rpc.get("id").cloned();
                    let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
                    // A notification has no id and gets no body.
                    let (status, reply) = match (id, method) {
                        (None, _) => (202, String::new()),
                        (Some(id), "initialize") => (200, json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": PROTOCOL_VERSION_FOR_TESTS, "capabilities": {},
                            "serverInfo": { "name": "fake", "version": "0" }
                        }}).to_string()),
                        (Some(id), "tools/list") => (200, json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "tools": [{ "name": "echo", "description": "Echo", "inputSchema": { "type": "object" } }]
                        }}).to_string()),
                        (Some(id), "tools/call") => (200, json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "echoed": rpc.pointer("/params/arguments").cloned().unwrap_or(Value::Null)
                        }}).to_string()),
                        (Some(id), other) => (200, json!({ "jsonrpc": "2.0", "id": id, "error": {
                            "code": -32601, "message": format!("no such method: {other}")
                        }}).to_string()),
                    };
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                                reply.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}/mcp"), seen)
    }

    #[cfg(not(target_arch = "wasm32"))]
    const PROTOCOL_VERSION_FOR_TESTS: &str = crate::mcp::PROTOCOL_VERSION;

    /// One stored row of the self-described provider, sealed the way a link
    /// flow seals it.
    #[cfg(not(target_arch = "wasm32"))]
    fn mcp_row(umk: &Key, payload: Value) -> String {
        let sealed = vault::seal_payload(umk, &payload.to_string());
        json!({ "items": [{
            "name": "stripe",
            "provider": "mcp",
            "label": "127.0.0.1",
            "blob": sealed.blob,
            "wrapped_dek": sealed.wrapped_dek,
            "key_id": "k1",
        }]})
        .to_string()
    }

    /// The whole device-side path for a self-described row: the call arrives
    /// on the socket, the row is opened, the ADDRESS IN THE PAYLOAD is where
    /// the MCP request goes — and with no credential in the payload, no
    /// credential header goes with it. The registry row says nothing about
    /// where to send; that this works at all is the delegation.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_self_described_row_is_called_at_the_address_in_its_payload() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let (endpoint, seen) = fake_mcp_server().await;
        let umk = Key::random();
        let row = mcp_row(&umk, json!({ "endpoint": endpoint }));
        let mock = spawn_mock(vec![
            ok(r#"{"claimed":true}"#), // ours
            ok(&row),                  // call_any reads the row
            ok(&row),                  // call() re-reads it before opening
            ok("null"),                // answerConnectionCall
        ]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk);

        handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-9","connection":"stripe","method":"echo","params":{"x":1}}}"#,
        )
        .await;

        let reqs = mock.requests.lock().unwrap();
        let answer = reqs.last().expect("an answer");
        assert_eq!(answer.path, "/answerConnectionCall");
        assert!(answer.body.contains(r#""echoed":{"x":1}"#), "{}", answer.body);
        assert!(!answer.body.contains("\"error\""), "{}", answer.body);

        let hdrs = seen.lock().unwrap();
        assert!(hdrs.len() >= 2, "initialize and the call both reached the server: {hdrs:?}");
        assert!(
            hdrs.iter().all(|h| !h.contains_key("authorization")),
            "no credential in the payload means no credential header, not an empty one: {hdrs:?}"
        );
        assert!(hdrs.iter().all(|h| h.get("mcp-protocol-version").is_some()));
    }

    /// The same path with a pasted token that rides in a custom header: the
    /// payload's `auth_header` decides the header, and the token goes verbatim
    /// — no `Bearer ` in front of an API key.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_self_described_rows_token_rides_in_the_header_its_payload_names() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let (endpoint, seen) = fake_mcp_server().await;
        let umk = Key::random();
        let row = mcp_row(
            &umk,
            json!({ "endpoint": endpoint, "access_token": "k-123", "auth_header": "X-Api-Key" }),
        );
        let mock = spawn_mock(vec![
            ok(r#"{"claimed":true}"#),
            ok(&row),
            ok(&row),
            ok("null"),
        ]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk);
        handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-10","connection":"stripe","method":"echo","params":{}}}"#,
        )
        .await;

        let hdrs = seen.lock().unwrap();
        assert!(hdrs.len() >= 2, "{hdrs:?}");
        for h in hdrs.iter() {
            assert_eq!(h.get("x-api-key").map(String::as_str), Some("k-123"), "{h:?}");
            assert!(!h.contains_key("authorization"), "{h:?}");
        }
    }

    // ── a computer of your own ──

    /// A stored `computer` row, sealed to `device_id` under this runtime's key.
    #[cfg(not(target_arch = "wasm32"))]
    fn computer_row(umk: &Key, device_id: &str) -> String {
        let sealed = vault::seal_payload(
            umk,
            &json!({ "device_id": device_id, "machine": "ops-mbp" }).to_string(),
        );
        json!({ "items": [{
            "name": "laptop",
            "provider": "computer",
            "label": "ops-mbp",
            "blob": sealed.blob,
            "wrapped_dek": sealed.wrapped_dek,
            "key_id": "k1",
        }]})
        .to_string()
    }

    /// An executor that records what it was asked to do and answers a fixed
    /// result — the host half, without a real process.
    #[cfg(not(target_arch = "wasm32"))]
    fn recording_executor() -> (Executor, std::sync::Arc<std::sync::Mutex<Vec<crate::computer::Job>>>)
    {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let exec: Executor = std::sync::Arc::new(move |job| {
            let sink = sink.clone();
            Box::pin(async move {
                sink.lock().unwrap().push(job);
                Ok(json!({ "exit_code": 0, "stdout": "hi\n" }))
            })
        });
        (exec, seen)
    }

    /// **The** invariant of this provider. The relay addresses an ACCOUNT, so
    /// every machine the person owns is offered every shell command they run —
    /// and a machine that is not the one named must not so much as claim it,
    /// let alone execute it. Losing this check does not read as a permission
    /// error; it reads as a build running in the wrong tree.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_shell_call_is_declined_by_every_machine_but_the_one_it_names() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let umk = Key::random();
        let mock = spawn_mock(vec![ok(&computer_row(&umk, "device-B"))]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk.clone());
        let (exec, seen) = recording_executor();
        rt.attach_computer("device-A", exec);

        let handled = handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-1","connection":"laptop","method":"shell.exec","params":{"cmd":"rm -rf build"}}}"#,
        )
        .await;

        assert!(handled, "it was ours — we just aren't the machine");
        assert!(seen.lock().unwrap().is_empty(), "nothing may run here");
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(
            reqs.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["/listConnections"],
            "declining happens BEFORE the claim — claiming would strand the call \
             on a machine that cannot run it"
        );
    }

    /// A daemon with no executor attached (an iOS app, a mac client — anything
    /// that links this core without being a shell host) is in exactly the same
    /// position as the wrong machine: silent, and out of the way.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_host_that_cannot_run_a_shell_never_claims_one() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let umk = Key::random();
        let mock = spawn_mock(vec![ok(&computer_row(&umk, "device-B"))]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk.clone());

        handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-2","connection":"laptop","method":"shell.exec","params":{"cmd":"ls"}}}"#,
        )
        .await;

        let reqs = mock.requests.lock().unwrap();
        assert!(
            !reqs.iter().any(|r| r.path == "/claimConnectionCall"),
            "a host with no way to run the job must not promise to"
        );
    }

    /// The machine that IS named claims, runs it, and answers — and the job it
    /// runs is the one that was asked for, parsed once in the core so every
    /// host cannot disagree about what the arguments meant.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn the_named_machine_claims_runs_and_answers() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let umk = Key::random();
        let row = computer_row(&umk, "device-A");
        let mock = spawn_mock(vec![
            ok(&row),                    // the pre-claim gate resolves the binding
            ok(r#"{"claimed":true}"#),   // ours
            ok(&row),                    // call_any re-reads the row it will open
            ok("null"),                  // answerConnectionCall
        ]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk.clone());
        let (exec, seen) = recording_executor();
        rt.attach_computer("device-A", exec);

        handle_event(
            &mut rt,
            r#"{"method":"events.connectionCall","params":{"call_id":"c-3","connection":"laptop","method":"shell.exec","params":{"cmd":"echo hi","cwd":"/tmp"}}}"#,
        )
        .await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[crate::computer::Job::Exec {
                cmd: "echo hi".into(),
                cwd: Some("/tmp".into()),
                timeout_ms: crate::computer::DEFAULT_EXEC_MS,
            }]
        );
        let reqs = mock.requests.lock().unwrap();
        let answer = reqs.last().expect("an answer");
        assert_eq!(answer.path, "/answerConnectionCall");
        assert!(answer.body.contains("\"stdout\""), "{}", answer.body);
        assert!(!answer.body.contains("\"error\""), "{}", answer.body);
    }

    /// `tools/list` is how an agent discovers what a granted connection can do.
    /// Codex answers empty by construction; a computer answers its shell verbs,
    /// or a bot granted a machine would be told the machine offers nothing.
    ///
    /// Synchronous, and that is not incidental: this file's async tests all
    /// carry `#[cfg(not(target_arch = "wasm32"))]` because `tokio` is a
    /// native-only dependency, and a `#[tokio::test]` without that gate breaks
    /// the WASM test build — which no amount of `cargo check --lib` will show
    /// you (it doesn't compile tests). Nothing here needs a runtime anyway: the
    /// catalog is a constant, and the point is that answering it depends on
    /// neither a socket nor a vault.
    #[test]
    fn a_computer_reports_its_shell_verbs_where_codex_reports_none() {
        let _reg = with_registry();
        assert_eq!(
            info("computer").native_api.as_deref(),
            Some(crate::computer::DRIVER)
        );
        assert!(
            info("codex-oauth").native_api.is_some(),
            "the contrast only means something while codex is native too"
        );

        let cat = crate::computer::catalog();
        let names: Vec<&str> = cat["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"shell.exec") && names.contains(&"shell.status"));
    }

    /// The sentence a caller gets when the machine is off. It has to name the
    /// machine: "no device answered" sends people looking at the network, and
    /// the fix is to open a laptop.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn calling_a_machine_that_isnt_this_one_says_which_machine() {
        use crate::testutil::{ok, spawn_mock};
        let _reg = with_registry();
        let umk = Key::random();
        let mock = spawn_mock(vec![ok(&computer_row(&umk, "device-B"))]);
        let mut rt = Runtime::new(&mock.base, "s_token", umk.clone());
        let (exec, _) = recording_executor();
        rt.attach_computer("device-A", exec);

        let err = rt
            .call_any("c-4", "laptop", "shell.exec", &json!({ "cmd": "ls" }), false)
            .await
            .unwrap_err();
        assert!(err.contains("ops-mbp"), "{err}");
        assert!(err.contains("mafold up"), "{err}");
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
