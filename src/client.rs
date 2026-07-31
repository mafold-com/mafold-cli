//! Shared Mafold client — HTTP API + WebSocket URL. Account-symmetric, so the
//! same client serves both the human-facing commands and the agent daemon.

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Tries for an idempotent write before giving up (400ms → 800ms → 1.6s → 3.2s:
/// ~6s of cover). Sized for the failure actually observed — a short uplink blip
/// / proxy TLS reset — not for a long outage, where the server-side stale-draft
/// reaper (`mafold-api`'s presence sweep) is the backstop instead.
const RETRY_ATTEMPTS: u32 = 5;

/// Outcome of `me_probed`: the identity, or a definitive auth rejection.
pub enum MeProbe {
    Me(Value),
    AuthRejected,
}

/// Where a message goes. In a forum the conversation is only HALF an address:
/// the channel, the thread it hangs under and the message it answers each narrow
/// it further, and every one of them is part of "reply where you were asked".
/// Carrying them in one value — built from the triggering message and passed
/// down — is what keeps a reply on the surface it belongs to; the four
/// half-addressed send helpers this replaced each dropped whatever they had no
/// parameter for (see `Client::send_to`).
#[derive(Clone, Copy, Default)]
pub struct Dest<'a> {
    pub chat_id: &'a str,
    /// The forum channel; None = the `#all` main timeline.
    pub channel_id: Option<&'a str>,
    /// The thread root this message hangs under; None = the timeline itself.
    pub thread_root_id: Option<&'a str>,
    /// The message being answered (renders as a quote), if any.
    pub reply_to_message_id: Option<&'a str>,
}

impl<'a> Dest<'a> {
    /// A conversation's main timeline — narrow it with the builders below.
    pub fn chat(chat_id: &'a str) -> Self {
        Self { chat_id, ..Default::default() }
    }
    /// …in this forum channel (None = `#all`).
    pub fn channel(mut self, channel_id: Option<&'a str>) -> Self {
        self.channel_id = channel_id;
        self
    }
    /// …under this thread root (None = the timeline itself). Any message in the
    /// thread works; the server normalizes it to the root.
    pub fn thread(mut self, thread_root_id: Option<&'a str>) -> Self {
        self.thread_root_id = thread_root_id;
        self
    }
    /// …as a reply to this message.
    pub fn reply_to(mut self, message_id: &'a str) -> Self {
        self.reply_to_message_id = Some(message_id);
        self
    }
}

#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub base: String,
    pub token: String,
}

impl Client {
    pub fn new(base: String, token: String) -> Self {
        // Bound only the CONNECT phase: a flaky network (observed: a local proxy
        // resetting fresh TLS connections) otherwise leaves a POST hanging
        // indefinitely. No overall request timeout — this client also streams the
        // multi-MB self-update download, which a blanket timeout would cut off.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, base, token }
    }

    /// The core's typed API handle for this base+token (base gains the `/api`
    /// prefix the core convention expects).
    fn api(&self) -> mafold_core::methods::ApiClient {
        mafold_core::methods::ApiClient::new(format!("{}/api", self.base), &self.token)
    }

    /// POST /api/<method> through the CORE's validated transport (method-name
    /// registry + pooled client + envelope unwrap live there now). Result is
    /// passed through as raw JSON — no typed round-trip, so fields the wire
    /// model hasn't caught up with can't silently vanish. The original
    /// `RpcError` rides along in the anyhow chain (see `is_connect_error`).
    async fn post(&self, method: &str, body: Value) -> Result<Value> {
        match self.api().call_raw(method, &body.to_string()).await {
            Ok(result) => serde_json::from_str(&result).with_context(|| format!("{method} returned non-JSON")),
            Err(e) => Err(anyhow::Error::new(e).context(format!("{method} failed"))),
        }
    }

    /// `post`, but retried with backoff on ANY transport failure — for calls
    /// that are IDEMPOTENT, where a duplicate delivery is a no-op.
    ///
    /// The `is_connect_error` split below exists because a *non*-idempotent call
    /// (botCreateDraft) can only be retried when the request provably never left
    /// the machine. A full-snapshot write has no such constraint: replaying it
    /// re-asserts the same final state. So these retry on timeouts and dropped
    /// responses too — the failure modes that a connect-only retry misses and
    /// that leave a turn's LAST push (the one that removes `{% generating %}`)
    /// silently dropped, stranding the bubble mid-animation forever.
    async fn post_idempotent(&self, method: &str, body: Value) -> Result<Value> {
        let mut delay = std::time::Duration::from_millis(400);
        for attempt in 1..=RETRY_ATTEMPTS {
            match self.post(method, body.clone()).await {
                Ok(v) => return Ok(v),
                // An API-level rejection (permission, 404, malformed) is a verdict,
                // not a blip: the server answered. Retrying just burns the backoff
                // and hides the real error. Only transport failures get another go.
                Err(e) if attempt == RETRY_ATTEMPTS || Self::is_api_error(&e) => return Err(e),
                Err(e) => {
                    eprintln!("{method} failed ({e}) — retry {attempt}/{RETRY_ATTEMPTS} in {delay:?}…");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        unreachable!("loop returns on the final attempt")
    }

    /// Did the SERVER answer with a refusal (as opposed to the request dying in
    /// transit)? Such a call must not be retried — the answer won't change.
    fn is_api_error(e: &anyhow::Error) -> bool {
        e.downcast_ref::<mafold_core::RpcError>()
            .is_some_and(|re| matches!(re, mafold_core::RpcError::Api(_)))
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

    /// POST /api/getUpdates — events with hub seq > `since` from the server's
    /// per-account replay buffer (256 newest per account). Items are
    /// `{seq, method, params}` in the exact shape WS frames arrive, so a
    /// reconnect replays what it missed through the same handling path.
    pub async fn get_updates(&self, since: u64) -> Result<Vec<Value>> {
        let v = self.post("getUpdates", json!({ "since": since })).await?;
        Ok(v["updates"].as_array().cloned().unwrap_or_default())
    }

    /// Recent messages in a conversation (`{ items: [Message] }`). The access
    /// gate drops non-owner messages so they never reach claude's resumed
    /// session; for a group turn the daemon re-fetches history to rebuild the
    /// multi-party context (see `recent_group_context`).
    pub async fn get_chat_history(&self, chat_id: &str, limit: usize, channel_id: Option<&str>) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "limit": limit });
        if let Some(ch) = channel_id {
            body["channel_id"] = json!(ch);
        }
        self.post("getChatHistory", body).await
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

    // ── forum channels (Telegram-Topics-style; see channels.rs) ──
    /// The forum's channels (`[Channel]`), decorated for the caller. `#all` is
    /// implicit (channel_id NULL) and never in this list.
    pub async fn list_channels(&self, chat_id: &str) -> Result<Value> {
        self.post("listChannels", json!({ "chat_id": chat_id })).await
    }
    pub async fn create_channel(&self, chat_id: &str, name: &str, icon: Option<&str>) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "name": name });
        if let Some(i) = icon {
            body["icon"] = json!(i);
        }
        self.post("createChannel", body).await
    }
    /// Partial edit: absent = unchanged; `icon: Some("")` clears the icon.
    pub async fn edit_channel(&self, chat_id: &str, channel_id: &str, name: Option<&str>, icon: Option<&str>) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "channel_id": channel_id });
        if let Some(n) = name {
            body["name"] = json!(n);
        }
        if let Some(i) = icon {
            body["icon"] = json!(i);
        }
        self.post("editChannel", body).await
    }
    /// Removes the channel AND its contents (messages, threads, read state).
    pub async fn delete_channel(&self, chat_id: &str, channel_id: &str) -> Result<Value> {
        self.post("deleteChannel", json!({ "chat_id": chat_id, "channel_id": channel_id })).await
    }
    pub async fn set_channel_closed(&self, chat_id: &str, channel_id: &str, closed: bool) -> Result<Value> {
        self.post("setChannelClosed", json!({ "chat_id": chat_id, "channel_id": channel_id, "closed": closed })).await
    }
    pub async fn set_channel_pinned(&self, chat_id: &str, channel_id: &str, pinned: bool) -> Result<Value> {
        self.post("setChannelPinned", json!({ "chat_id": chat_id, "channel_id": channel_id, "pinned": pinned })).await
    }
    pub async fn set_channel_archived(&self, chat_id: &str, channel_id: &str, archived: bool) -> Result<Value> {
        self.post("setChannelArchived", json!({ "chat_id": chat_id, "channel_id": channel_id, "archived": archived })).await
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

    /// This bot's own per-conversation config bag (`{ config: {key: value} }`)
    /// — the Customize sheet's chat scope. Callable with the bot's own token.
    pub async fn bot_conv_config(&self, chat_id: &str) -> Result<Value> {
        self.post("getBotConvConfig", json!({ "chat_id": chat_id })).await
    }

    /// The bot's OWNER-set config, callable by the bot itself. Returns `BotDetail`
    /// — `{ bot, config, config_schema, secret_schema, secrets }`. The daemon uses
    /// `config` (a `{key: value}` map of the owner's stored field values) to drive
    /// the harness defaults (model / system prompt / working dir).
    pub async fn bot(&self, username: &str) -> Result<Value> {
        self.post("getBot", json!({ "username": username })).await
    }

    /// Post a message at `dest` — THE send path.
    ///
    /// Every dimension the destination carries goes on the wire, so "answer
    /// where you were asked" is what a call site gets by default. This used to
    /// be four overlapping helpers (`send` / `send_threaded` / `send_in` /
    /// `send_reply_in`), each carrying only part of the address: picking one
    /// silently dropped whatever it had no parameter for, which is how `/usage`
    /// asked in #a kept answering in `#all`. A single destination-complete call
    /// makes that a compile-time impossibility rather than a review item.
    ///
    /// Go through this, never a hand-rolled body: a hand-written gate payload
    /// used conversation_id/content/reply_to_id instead of the API's
    /// chat_id/text/reply_to_message_id and silently 422'd for three releases
    /// (0.9.56→0.9.63; diagnosed in the field by @linsky:opus48).
    pub async fn send_to(&self, dest: Dest<'_>, text: &str) -> Result<Value> {
        let mut body = json!({ "chat_id": dest.chat_id, "text": text });
        if let Some(ch) = dest.channel_id {
            body["channel_id"] = json!(ch);
        }
        if let Some(root) = dest.thread_root_id {
            body["thread_root_id"] = json!(root);
        }
        if let Some(rid) = dest.reply_to_message_id {
            body["reply_to_message_id"] = json!(rid);
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

    /// Did this request die in the CONNECT phase — i.e. it never left this
    /// machine? Only these failures are safe to blindly retry: the server saw
    /// nothing, so a retry can't duplicate anything. (A timeout/dropped-response
    /// is NOT safe for non-idempotent calls like botCreateDraft — the request may
    /// have landed, and a retry would leave an orphaned empty draft bubble.)
    /// The classification now comes from the core's structured `RpcError`.
    fn is_connect_error(e: &anyhow::Error) -> bool {
        e.downcast_ref::<mafold_core::RpcError>()
            .is_some_and(|re| matches!(re, mafold_core::RpcError::Connect(_)))
    }

    // ── bot streaming write API (used by the agent daemon) ──
    /// Open a streaming draft. When `thread_root_id` is set, the streamed reply
    /// lands in that thread instead of the main channel.
    ///
    /// This POST is the reply pipeline's single point of failure: it runs before
    /// any state exists, and `handle()` propagates its error — so one flaky
    /// connect used to silently eat the user's message (bot "online but never
    /// replies"). Connect-phase failures are retried a couple of times (observed
    /// real-world cause: a local proxy killing fresh TLS connections right after
    /// the WS reconnects).
    pub async fn create_draft(&self, chat_id: &str, thread_root_id: Option<&str>, channel_id: Option<&str>) -> Result<String> {
        // TYPED through the core: ids parse to real Uuids up front (a malformed
        // id fails here, not as a server 400), and the result is a wire::Message.
        let chat = uuid::Uuid::parse_str(chat_id).context("botCreateDraft: bad chat_id")?;
        let root = thread_root_id
            .map(uuid::Uuid::parse_str)
            .transpose()
            .context("botCreateDraft: bad thread_root_id")?;
        let channel = channel_id
            .map(uuid::Uuid::parse_str)
            .transpose()
            .context("botCreateDraft: bad channel_id")?;
        let api = self.api();
        let mut delay = std::time::Duration::from_millis(500);
        let mut attempt = 0;
        let draft = loop {
            match api.bot_create_draft(chat, root, channel).await {
                Ok(m) => break m,
                Err(e @ mafold_core::RpcError::Connect(_)) if attempt < 2 => {
                    let _ = e;
                    attempt += 1;
                    eprintln!("botCreateDraft connect failed (attempt {attempt}/3) — retrying in {delay:?}…");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(anyhow::Error::new(e).context("botCreateDraft failed")),
            }
        };
        Ok(draft.id.to_string())
    }
    pub async fn append_delta(&self, message_id: &str, delta: &str) -> Result<()> {
        self.post("botAppendDelta", json!({ "message_id": message_id, "delta": delta })).await?;
        Ok(())
    }

    /// Replace a streaming draft's FULL content (Telegram `sendMessageDraft`
    /// style — each push is the complete snapshot, so we can rewrite earlier
    /// output, e.g. swap a trailing `{% generating %}` card for `{% result %}`).
    ///
    /// RETRIED (see `post_idempotent`): a snapshot write is idempotent, and the
    /// final one of a turn is what takes the `{% generating %}` card away. Losing
    /// it to a two-second uplink blip left the bubble animating forever with a
    /// Stop button that could never resolve — the failure this retry exists for.
    pub async fn edit_draft(&self, message_id: &str, content: &str) -> Result<()> {
        self.post_idempotent("botEditDraft", json!({ "message_id": message_id, "content": content })).await?;
        Ok(())
    }
    /// RETRIED: finalizing an already-finalized message is a no-op server-side,
    /// and an unfinalized draft is precisely the "forever generating" bubble.
    pub async fn finalize(&self, message_id: &str) -> Result<()> {
        self.post_idempotent("botFinalize", json!({ "message_id": message_id })).await?;
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

    /// Retract a card from YOUR scope — one version, or the whole tag. Used to
    /// clear a family shadow so the tag resolves to the global card again.
    pub async fn unpublish_card(&self, tag: &str, version: Option<&str>) -> Result<Value> {
        self.post("unpublishCard", json!({ "tag": tag, "version": version })).await
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
