//! Shared Mafold client — HTTP API + WebSocket URL. Account-symmetric, so the
//! same client serves both the human-facing commands and the agent daemon.

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Outcome of `me_probed`: the identity, or a definitive auth rejection.
pub enum MeProbe {
    Me(Value),
    AuthRejected,
}

#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub base: String,
    pub token: String,
}

impl Client {
    pub fn new(base: String, token: String) -> Self {
        Self { http: reqwest::Client::new(), base, token }
    }

    /// POST /api/<method>, unwrap the `{ok, result}` envelope or bail.
    async fn post(&self, method: &str, body: Value) -> Result<Value> {
        let v: Value = self
            .http
            .post(format!("{}/api/{method}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} request failed"))?
            .json()
            .await
            .with_context(|| format!("{method} returned non-JSON"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            anyhow::bail!("{method}: {}", v["description"].as_str().unwrap_or("error"));
        }
        Ok(v["result"].clone())
    }

    /// Generic authenticated RPC for callers outside this module (login, report…).
    pub async fn call(&self, method: &str, body: Value) -> Result<Value> { self.post(method, body).await }

    pub async fn me(&self) -> Result<Value> { self.post("getMe", json!({})).await }

    /// `getMe`, but with an AUTH REJECTION (401/403 — token revoked / bot
    /// deleted) split out from ordinary failures, so a daemon whose bot was
    /// deleted while it was offline can deprovision at startup instead of
    /// crash-looping under the supervisor forever.
    pub async fn me_probed(&self) -> Result<MeProbe> {
        let resp = self
            .http
            .post(format!("{}/api/getMe", self.base))
            .bearer_auth(&self.token)
            .json(&json!({}))
            .send()
            .await
            .context("getMe request failed")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Ok(MeProbe::AuthRejected);
        }
        let v: Value = resp.json().await.context("getMe returned non-JSON")?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            anyhow::bail!("getMe: {}", v["description"].as_str().unwrap_or("error"));
        }
        Ok(MeProbe::Me(v["result"].clone()))
    }
    pub async fn chats(&self) -> Result<Value> { self.post("getChats", json!({})).await }

    /// A single conversation (`{ id, kind, participants, … }`) — used to tell a
    /// group from a DM for the group reply gate.
    pub async fn get_chat(&self, chat_id: &str) -> Result<Value> {
        self.post("getChat", json!({ "chat_id": chat_id })).await
    }

    /// Recent messages in a conversation (`{ items: [Message] }`). The access
    /// gate drops non-owner messages so they never reach claude's resumed
    /// session; for a group turn the daemon re-fetches history to rebuild the
    /// multi-party context (see `recent_group_context`).
    pub async fn get_chat_history(&self, chat_id: &str, limit: usize) -> Result<Value> {
        self.post("getChatHistory", json!({ "chat_id": chat_id, "limit": limit })).await
    }

    /// A thread's messages (root + replies) — used to rebuild context when the
    /// bot is @-mentioned INSIDE a thread (thread replies aren't in the channel's
    /// main timeline, so getChatHistory alone misses them).
    pub async fn get_thread_messages(&self, chat_id: &str, root_message_id: &str, limit: usize) -> Result<Value> {
        self.post(
            "getThreadMessages",
            json!({ "chat_id": chat_id, "root_message_id": root_message_id, "limit": limit }),
        )
        .await
    }

    /// Per-group bot dispatch settings (`{ items: [{ bot, always_on }] }`) —
    /// tells the daemon whether it's set to always-on in this group.
    pub async fn group_bots(&self, chat_id: &str) -> Result<Value> {
        self.post("getGroupBots", json!({ "chat_id": chat_id })).await
    }

    // ── shared-room CRDT relay (the AI's room peer; see room.rs) ──
    pub async fn room_changes(&self, conv: &str, app: &str) -> Result<Value> {
        self.post("roomChanges", json!({ "conv": conv, "app": app })).await
    }
    pub async fn room_change(&self, conv: &str, app: &str, changes: Vec<String>) -> Result<Value> {
        self.post("roomChange", json!({ "conv": conv, "app": app, "changes": changes })).await
    }
    pub async fn list_installs(&self, conv: &str) -> Result<Value> {
        self.post("listInstalls", json!({ "conversation_id": conv })).await
    }

    /// The bot's OWNER-set config, callable by the bot itself. Returns `BotDetail`
    /// — `{ bot, config, config_schema, secret_schema, secrets }`. The daemon uses
    /// `config` (a `{key: value}` map of the owner's stored field values) to drive
    /// the harness defaults (model / system prompt / working dir).
    pub async fn bot(&self, username: &str) -> Result<Value> {
        self.post("getBot", json!({ "username": username })).await
    }

    pub async fn send(&self, chat_id: &str, text: &str) -> Result<Value> {
        self.send_threaded(chat_id, text, None).await
    }

    /// Send a message, optionally as a Slack-style thread reply under
    /// `thread_root_id` (any message in the thread; the server normalizes it).
    pub async fn send_threaded(&self, chat_id: &str, text: &str, thread_root_id: Option<&str>) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if let Some(root) = thread_root_id {
            body["thread_root_id"] = json!(root);
        }
        self.post("sendMessage", body).await
    }

    /// Publish this bot's slash commands (the chat command panel).
    pub async fn set_commands(&self, commands: Value) -> Result<()> {
        self.post("setBotCommands", json!({ "commands": commands })).await?;
        Ok(())
    }

    /// Download a media attachment. The path comes from the server, so it's NOT
    /// trusted as an arbitrary URL: only a relative `/media/…`-style path (joined
    /// onto our own `base`) or an absolute URL on the SAME origin as `base` is
    /// fetched. Anything else (a foreign `http(s)://` host) is rejected so a
    /// crafted attachment URL can't make the daemon do an SSRF request.
    pub async fn download(&self, path: &str) -> Result<Vec<u8>> {
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            // Absolute URL → only allow it if it's on our own API origin.
            if !same_origin(&self.base, path) {
                anyhow::bail!("refusing to fetch attachment from a non-Mafold origin: {path}");
            }
            path.to_string()
        } else if path.starts_with('/') {
            // A relative server path (e.g. `/media/…`) → resolve against base.
            format!("{}{}", self.base, path)
        } else {
            anyhow::bail!("refusing to fetch attachment from a non-relative, non-Mafold URL: {path}");
        };
        let bytes = self.http.get(&url).send().await?.error_for_status()?.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Resolve a chat argument: a conversation UUID is used as-is; anything else
    /// is treated as a username (`@name` or `name`) → open/find the DM.
    pub async fn resolve_chat(&self, arg: &str) -> Result<String> {
        let a = arg.trim_start_matches('@');
        if a.len() == 36 && a.matches('-').count() == 4 {
            return Ok(a.to_string());
        }
        let conv = self.post("startChat", json!({ "user_ids": [a] })).await?;
        Ok(conv["id"].as_str().context("startChat: no conversation id")?.to_string())
    }

    // ── bot streaming write API (used by the agent daemon) ──
    /// Open a streaming draft. When `thread_root_id` is set, the streamed reply
    /// lands in that thread instead of the main channel.
    pub async fn create_draft(&self, chat_id: &str, thread_root_id: Option<&str>) -> Result<String> {
        let mut body = json!({ "chat_id": chat_id });
        if let Some(root) = thread_root_id {
            body["thread_root_id"] = json!(root);
        }
        let r = self.post("botCreateDraft", body).await?;
        Ok(r["id"].as_str().context("botCreateDraft: no id")?.to_string())
    }
    pub async fn append_delta(&self, message_id: &str, delta: &str) -> Result<()> {
        self.post("botAppendDelta", json!({ "message_id": message_id, "delta": delta })).await?;
        Ok(())
    }

    /// Replace a streaming draft's FULL content (Telegram `sendMessageDraft`
    /// style — each push is the complete snapshot, so we can rewrite earlier
    /// output, e.g. swap a trailing `{% generating %}` card for `{% result %}`).
    pub async fn edit_draft(&self, message_id: &str, content: &str) -> Result<()> {
        self.post("botEditDraft", json!({ "message_id": message_id, "content": content })).await?;
        Ok(())
    }
    pub async fn finalize(&self, message_id: &str) -> Result<()> {
        self.post("botFinalize", json!({ "message_id": message_id })).await?;
        Ok(())
    }

    /// Push a directed alert popup to a single user (Telegram answerCallbackQuery
    /// `show_alert` analog). Used to tell a non-allow-listed user their Stop
    /// request was denied. `level` ∈ {info, success, error}.
    pub async fn push_alert(&self, to: &str, title: Option<&str>, text: &str, level: &str) -> Result<()> {
        let mut body = json!({ "to_username": to, "text": text, "level": level });
        if let Some(t) = title {
            body["title"] = json!(t);
        }
        self.post("pushAlert", body).await?;
        Ok(())
    }

    /// Answer a pending inline query (the user is typing `@me …`). `results` are
    /// message bodies — each may carry `{% card %}` tags; the client shows them as
    /// pickable suggestions and sends the chosen one.
    pub async fn answer_inline_query(&self, query_id: &str, results: Vec<String>) -> Result<()> {
        self.post("answerInlineQuery", json!({ "query_id": query_id, "results": results })).await?;
        Ok(())
    }

    // ── developer card registry ──
    /// Publish a compiled card bundle. `meta` carries tag/version/displayName.
    pub async fn publish_card(&self, meta: &Value, bundle: Vec<u8>) -> Result<Value> {
        let form = reqwest::multipart::Form::new()
            .text("meta", serde_json::to_string(meta)?)
            .part(
                "bundle",
                reqwest::multipart::Part::bytes(bundle)
                    .file_name("card.js")
                    .mime_str("application/javascript")?,
            );
        let v: Value = self
            .http
            .post(format!("{}/api/publishCard", self.base))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("publishCard request failed")?
            .json()
            .await
            .context("publishCard returned non-JSON")?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            anyhow::bail!("publishCard: {}", v["description"].as_str().unwrap_or("error"));
        }
        Ok(v["result"].clone())
    }

    /// The caller's own cards plus global cards.
    pub async fn list_cards(&self) -> Result<Value> {
        self.post("listCards", json!({})).await
    }

    // ── developer mini-app registry ──
    /// Publish a compiled mini-app bundle. `meta` is the full AppManifest JSON
    /// (carrying `id` = `owner/slug` and `version`); the server gates publish on
    /// owning the `owner` namespace. Mirrors `publish_card` (different route, a
    /// manifest instead of card meta).
    pub async fn publish_app(&self, meta: &Value, bundle: Vec<u8>) -> Result<Value> {
        let form = reqwest::multipart::Form::new()
            .text("meta", serde_json::to_string(meta)?)
            .part(
                "bundle",
                reqwest::multipart::Part::bytes(bundle)
                    .file_name("app.js")
                    .mime_str("application/javascript")?,
            );
        let v: Value = self
            .http
            .post(format!("{}/api/publishApp", self.base))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("publishApp request failed")?
            .json()
            .await
            .context("publishApp returned non-JSON")?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            anyhow::bail!("publishApp: {}", v["description"].as_str().unwrap_or("error"));
        }
        Ok(v["result"].clone())
    }

    /// Upload a media asset (e.g. an app logo) via `/api/uploadFile`. Returns the
    /// bare `MediaUploadResponse` (`{media_id, url, mime, size_bytes, filename?}`)
    /// — NOT the `{ok,result}` envelope — with `url` = a served `/media/…` path.
    pub async fn upload_media(&self, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<Value> {
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(filename.to_string())
                .mime_str(mime)?,
        );
        let resp = self
            .http
            .post(format!("{}/api/uploadFile", self.base))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("uploadFile request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("uploadFile: reading body")?;
        if !status.is_success() {
            anyhow::bail!("uploadFile failed ({status}): {text}");
        }
        serde_json::from_str(&text).context("uploadFile returned non-JSON")
    }

    /// Every mini-app whose namespace the caller may manage (the "my apps" view).
    pub async fn list_apps(&self) -> Result<Value> {
        self.post("listApps", json!({})).await
    }

    /// Take down a mini-app the caller owns (all versions). Returns `{removed}`.
    pub async fn remove_app(&self, id: &str) -> Result<Value> {
        self.post("removeApp", json!({ "id": id })).await
    }

    // ── cloud language-pack registry (i18n) ──
    /// Publish one language pack (first-party publisher only). `body` carries
    /// `lang_code`, `version`, `name?`, `rtl?`, `strings`. Returns `{lang_code,
    /// keyset, version, url}`.
    pub async fn publish_langpack(&self, body: Value) -> Result<Value> {
        self.post("publishLangPack", body).await
    }
    /// Resolve newest (or pinned) packs — used to read the current server version.
    pub async fn resolve_langpacks(&self, requests: Value) -> Result<Value> {
        self.post("resolveLangPacks", json!({ "requests": requests })).await
    }
    /// The languages the server currently serves (newest version each).
    pub async fn list_languages(&self) -> Result<Value> {
        self.post("listLanguages", json!({})).await
    }

    fn ws_url(&self) -> String {
        let ws = self.base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!("{ws}/api/ws")
    }

    /// The WS handshake request — sends the bot token via an `Authorization:
    /// Bearer` header instead of the URL query string (so the secret doesn't sit
    /// in logs / proxies). Pass this straight to `connect_async`.
    pub fn ws_request(&self) -> tokio_tungstenite::tungstenite::ClientRequestBuilder {
        let uri: tokio_tungstenite::tungstenite::http::Uri = self
            .ws_url()
            .parse()
            .expect("ws url should be a valid URI");
        tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {}", self.token))
    }
}

/// Do two URLs share the same origin (scheme + host + port)? Used to keep the
/// attachment downloader from following an absolute URL to a foreign host.
fn same_origin(base: &str, other: &str) -> bool {
    fn origin(u: &str) -> Option<(String, String)> {
        // scheme://host[:port]/…  → (scheme, host[:port]) lowercased.
        let (scheme, rest) = u.split_once("://")?;
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() {
            return None;
        }
        Some((scheme.to_lowercase(), authority.to_lowercase()))
    }
    match (origin(base), origin(other)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}
