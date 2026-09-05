//! Shared Mafold client — HTTP API + WebSocket URL. Account-symmetric, so the
//! same client serves both the human-facing commands and the agent daemon.

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Tries for an idempotent write before giving up (400ms → 800ms → 1.6s → 3.2s:
/// ~6s of cover). Sized for the failure actually observed — a short uplink blip
/// / proxy TLS reset — not for a long outage, where the server-side stale-draft
/// reaper (`mafold-api`'s presence sweep) is the backstop instead.
const RETRY_ATTEMPTS: u32 = 5;

/// Tries for fetching a media attachment (600ms → 1.2s of cover). Small on
/// purpose: the user is waiting on the reply this attachment belongs to, so a
/// long backoff here would trade a missing image for a late answer.
const MEDIA_ATTEMPTS: u32 = 3;

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
        Self {
            chat_id,
            ..Default::default()
        }
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
            Ok(result) => {
                serde_json::from_str(&result).with_context(|| format!("{method} returned non-JSON"))
            }
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
    /// that leave a turn's LAST push (the one that removes `{% mafold/generating %}`)
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
                    eprintln!(
                        "{method} failed ({e}) — retry {attempt}/{RETRY_ATTEMPTS} in {delay:?}…"
                    );
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
    pub async fn call(&self, method: &str, body: Value) -> Result<Value> {
        self.post(method, body).await
    }

    pub async fn me(&self) -> Result<Value> {
        self.post("getMe", json!({})).await
    }

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
    pub async fn chats(&self) -> Result<Value> {
        self.post("getChats", json!({})).await
    }

    /// One account (`{ username, display_name, language, … }`). The daemon reads
    /// the OWNER's `language` — the cloud i18n setting every other Mafold client
    /// follows — to know which language to introduce itself in. (The api names
    /// the field `user_id` but accepts a username there.)
    pub async fn get_user(&self, username: &str) -> Result<Value> {
        self.post("getUser", json!({ "user_id": username })).await
    }

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
    pub async fn get_chat_history(
        &self,
        chat_id: &str,
        limit: usize,
        channel_id: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "limit": limit });
        if let Some(ch) = channel_id {
            body["channel_id"] = json!(ch);
        }
        self.post("getChatHistory", body).await
    }

    /// A thread's messages (root + replies) — used to rebuild context when the
    /// bot is @-mentioned INSIDE a thread (thread replies aren't in the channel's
    /// main timeline, so getChatHistory alone misses them).
    pub async fn get_thread_messages(
        &self,
        chat_id: &str,
        root_message_id: &str,
        limit: usize,
    ) -> Result<Value> {
        self.post(
            "getThreadMessages",
            json!({ "chat_id": chat_id, "root_message_id": root_message_id, "limit": limit }),
        )
        .await
    }

    /// Per-group bot dispatch settings (`{ items: [{ bot, always_on }] }`) —
    /// tells the daemon whether it's set to always-on in this group.
    pub async fn group_bots(&self, chat_id: &str) -> Result<Value> {
        self.post("getGroupBots", json!({ "chat_id": chat_id }))
            .await
    }

    // ── forum channels (Telegram-Topics-style; see channels.rs) ──
    /// The forum's channels (`[Channel]`), decorated for the caller. `#all` is
    /// implicit (channel_id NULL) and never in this list.
    pub async fn list_channels(&self, chat_id: &str) -> Result<Value> {
        self.post("listChannels", json!({ "chat_id": chat_id }))
            .await
    }
    pub async fn create_channel(
        &self,
        chat_id: &str,
        name: &str,
        icon: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id, "name": name });
        if let Some(i) = icon {
            body["icon"] = json!(i);
        }
        self.post("createChannel", body).await
    }
    /// Partial edit: absent = unchanged; `icon: Some("")` clears the icon.
    pub async fn edit_channel(
        &self,
        chat_id: &str,
        channel_id: &str,
        name: Option<&str>,
        icon: Option<&str>,
    ) -> Result<Value> {
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
        self.post(
            "deleteChannel",
            json!({ "chat_id": chat_id, "channel_id": channel_id }),
        )
        .await
    }
    pub async fn set_channel_closed(
        &self,
        chat_id: &str,
        channel_id: &str,
        closed: bool,
    ) -> Result<Value> {
        self.post(
            "setChannelClosed",
            json!({ "chat_id": chat_id, "channel_id": channel_id, "closed": closed }),
        )
        .await
    }
    pub async fn set_channel_pinned(
        &self,
        chat_id: &str,
        channel_id: &str,
        pinned: bool,
    ) -> Result<Value> {
        self.post(
            "setChannelPinned",
            json!({ "chat_id": chat_id, "channel_id": channel_id, "pinned": pinned }),
        )
        .await
    }
    pub async fn set_channel_archived(
        &self,
        chat_id: &str,
        channel_id: &str,
        archived: bool,
    ) -> Result<Value> {
        self.post(
            "setChannelArchived",
            json!({ "chat_id": chat_id, "channel_id": channel_id, "archived": archived }),
        )
        .await
    }

    // ── shared-room CRDT relay (the AI's room peer; see room.rs) ──
    pub async fn room_changes(&self, conv: &str, app: &str) -> Result<Value> {
        self.post("roomChanges", json!({ "conv": conv, "app": app }))
            .await
    }
    pub async fn room_change(&self, conv: &str, app: &str, changes: Vec<String>) -> Result<Value> {
        self.post(
            "roomChange",
            json!({ "conv": conv, "app": app, "changes": changes }),
        )
        .await
    }
    pub async fn list_installs(&self, conv: &str) -> Result<Value> {
        self.post("listInstalls", json!({ "conversation_id": conv }))
            .await
    }

    /// This bot's own per-conversation config bag (`{ config: {key: value} }`)
    /// — the Customize sheet's chat scope. Callable with the bot's own token.
    pub async fn bot_conv_config(&self, chat_id: &str) -> Result<Value> {
        self.post("getBotConvConfig", json!({ "chat_id": chat_id }))
            .await
    }

    /// **What configuration this bot is actually running under, for this turn.**
    ///
    /// A value can be pinned to a conversation, to a person, to both, or to
    /// neither, and the server owns the ladder that picks between them
    /// (`resolveBotConfig`). The daemon used to walk part of it by hand — chat
    /// bag over owner defaults, and the per-USER bag not at all, so a member's
    /// own Customize settings were stored, shown, and never read. Asking is both
    /// shorter and the only way this stays in step with the other clients.
    ///
    /// Returns `{ bot, fields: {key: value}, sources: {key: {conv, user}} }`
    /// with every layer already merged per key.
    pub async fn resolved_config(&self, chat_id: &str, user: Option<&str>) -> Result<Value> {
        let mut body = json!({ "chat_id": chat_id });
        if let Some(u) = user {
            body["user"] = json!(u);
        }
        self.post("resolveBotConfig", body).await
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
        self.post("setBotCommands", json!({ "commands": commands }))
            .await?;
        Ok(())
    }

    /// Download a media attachment. The path comes from the server, so it's NOT
    /// trusted as an arbitrary URL: only a relative `/media/…`-style path (joined
    /// onto our own `base`) or an absolute URL on an ALLOWED media origin is
    /// fetched. Anything else (a foreign `http(s)://` host) is rejected so a
    /// crafted attachment URL can't make the daemon do an SSRF request.
    ///
    /// RETRIED: a GET is idempotent, so re-issuing one costs nothing — and NOT
    /// re-issuing costs the user's attachment. A single 500 from the media
    /// redirect (2026-08-18, the api blinking under a deploy) was enough to drop
    /// a screenshot out of the prompt, leaving the model to answer a message it
    /// could not see, with one log line as the only trace. A 4xx is the server
    /// saying the thing is really gone — that one is not worth repeating.
    pub async fn download(&self, path: &str) -> Result<Vec<u8>> {
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            if !self.media_origin_allowed(path) {
                anyhow::bail!("refusing to fetch attachment from a non-Mafold origin: {path}");
            }
            path.to_string()
        } else if path.starts_with('/') {
            // A relative server path (e.g. `/media/…`) → resolve against base.
            format!("{}{}", self.base, path)
        } else {
            anyhow::bail!(
                "refusing to fetch attachment from a non-relative, non-Mafold URL: {path}"
            );
        };
        let mut delay = std::time::Duration::from_millis(600);
        let mut attempt = 1;
        loop {
            let got = async {
                let resp = self.http.get(&url).send().await?.error_for_status()?;
                Ok::<_, reqwest::Error>(resp.bytes().await?)
            }
            .await;
            match got {
                Ok(bytes) => return Ok(bytes.to_vec()),
                Err(e)
                    if attempt < MEDIA_ATTEMPTS
                        && !e.status().is_some_and(|s| s.is_client_error()) =>
                {
                    eprintln!(
                        "attachment fetch failed ({e}) — retry {attempt}/{} in {delay:?}…",
                        MEDIA_ATTEMPTS - 1
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
                Err(e) => return Err(anyhow::Error::new(e)),
            }
        }
    }

    /// Resolve a chat argument: a conversation UUID is used as-is; anything else
    /// is treated as a username (`@name` or `name`) → open/find the DM.
    pub async fn resolve_chat(&self, arg: &str) -> Result<String> {
        let a = arg.trim_start_matches('@');
        if a.len() == 36 && a.matches('-').count() == 4 {
            return Ok(a.to_string());
        }
        let conv = self.post("startChat", json!({ "user_ids": [a] })).await?;
        Ok(conv["id"]
            .as_str()
            .context("startChat: no conversation id")?
            .to_string())
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

    /// Did the far end FALL OVER, as opposed to deliberately saying no? A 5xx
    /// envelope, a gateway's non-JSON error page, a torn socket or a timeout all
    /// mean "the same request could work a second from now"; a 4xx or an unknown
    /// method mean "this will never work".
    ///
    /// Note the deliberate split from `is_api_error`, which treats EVERY
    /// `{ok:false}` as a verdict: that is the right rule for a call being
    /// retried freely, but a 5xx envelope is the server admitting it broke, not
    /// answering. This one is only consulted where a single extra attempt buys
    /// something that matters (see `create_draft`).
    fn is_server_blip(e: &mafold_core::RpcError) -> bool {
        use mafold_core::RpcError as R;
        match e {
            // Timeout, dropped response, or a non-JSON body — which the core
            // formats as "<method>: HTTP <status> …", so a deliberate 4xx is
            // still recognizable in here and stays excluded.
            R::Transport(s) => !s.contains("HTTP 4"),
            R::Api(env) => serde_json::from_str::<Value>(env)
                .ok()
                .and_then(|v| v.get("error_code").and_then(Value::as_u64))
                .is_some_and(|c| (500..600).contains(&c)),
            // Connect has its own (freely repeatable) retry in `create_draft`;
            // an unknown method is a client/server version gap, never a blip.
            R::Connect(_) | R::UnknownMethod(_) => false,
        }
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
    pub async fn create_draft(
        &self,
        chat_id: &str,
        thread_root_id: Option<&str>,
        channel_id: Option<&str>,
    ) -> Result<String> {
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
        let mut connect_tries = 0;
        let mut blip_retried = false;
        let draft = loop {
            let err = match api.bot_create_draft(chat, root, channel).await {
                Ok(m) => break m,
                Err(e) => e,
            };
            // Never left this machine → a retry cannot duplicate anything.
            if matches!(err, mafold_core::RpcError::Connect(_)) && connect_tries < 2 {
                connect_tries += 1;
                eprintln!(
                    "botCreateDraft connect failed (attempt {connect_tries}/3: {err}) — retrying in {delay:?}…"
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
                continue;
            }
            // The far end fell over rather than said no. This is NOT provably
            // duplicate-free — the draft may have been created and only the
            // answer lost — so it gets exactly ONE more attempt, and the worst
            // case is a single empty bubble left behind. The failure it exists
            // for is strictly worse: on 2026-08-18 one 500 here ate the user's
            // message whole, and because the WS cursor is pinned before
            // `handle()` runs, nothing ever replayed it.
            if !blip_retried && Self::is_server_blip(&err) {
                blip_retried = true;
                eprintln!("botCreateDraft failed ({err}) — server-side blip, one more try in 2s…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            return Err(anyhow::Error::new(err).context("botCreateDraft failed"));
        };
        Ok(draft.id.to_string())
    }

    /// A turn died BEFORE it had a bubble to die in — say so where it was asked.
    ///
    /// Every other failure inside a turn is surfaced in the draft itself (see
    /// `handle`'s "ALWAYS finalize"). A `create_draft` failure has no draft to
    /// be surfaced in, so it used to leave exactly nothing: an online bot that
    /// read the message and never spoke, while the log kept the reason to
    /// itself. Nothing replays the message either (the cursor is pinned before
    /// the turn runs), so the ONLY recovery is the user sending it again — which
    /// they can't know to do unless we say it.
    ///
    /// Best-effort by design: one plain send, no retry. If the API is down hard
    /// this fails too, and the caller's log line is the last word.
    pub async fn announce_lost_turn(&self, dest: Dest<'_>, why: &str) -> Result<()> {
        // Trimmed: the point of the bubble is "send it again", not a stack trace.
        let why: String = why.chars().take(200).collect();
        self.send_to(
            dest,
            &format!(
                "⚠️ I couldn't open a reply draft for your last message, so it was never processed \
                 — and it won't be retried automatically. Please send it again.\n\n`{why}`"
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn append_delta(&self, message_id: &str, delta: &str) -> Result<()> {
        self.post(
            "botAppendDelta",
            json!({ "message_id": message_id, "delta": delta }),
        )
        .await?;
        Ok(())
    }

    /// Replace a streaming draft's FULL content (Telegram `sendMessageDraft`
    /// style — each push is the complete snapshot, so we can rewrite earlier
    /// output, e.g. swap a trailing `{% mafold/generating %}` card for `{% mafold/result %}`).
    ///
    /// RETRIED (see `post_idempotent`): a snapshot write is idempotent, and the
    /// final one of a turn is what takes the `{% mafold/generating %}` card away. Losing
    /// it to a two-second uplink blip left the bubble animating forever with a
    /// Stop button that could never resolve — the failure this retry exists for.
    pub async fn edit_draft(&self, message_id: &str, content: &str) -> Result<()> {
        self.post_idempotent(
            "botEditDraft",
            json!({ "message_id": message_id, "content": content }),
        )
        .await?;
        Ok(())
    }
    /// Attach media to one of OUR messages (draft or already delivered). The
    /// server appends, de-duped by attachment id — which is what makes the
    /// retry below safe, and what keeps this independent of the text snapshots
    /// `edit_draft` is pushing in parallel.
    ///
    /// RETRIED: an image that silently fails to land is exactly the bug this
    /// whole path exists to fix — the reply says "已生成" and nothing arrives.
    pub async fn attach(&self, message_id: &str, attachments: Value) -> Result<()> {
        self.post_idempotent("botAttach", json!({ "message_id": message_id, "attachments": attachments })).await?;
        Ok(())
    }

    /// Upload a local file and attach it to `message_id`, returning the
    /// attachment JSON that landed. The one place that turns "a path on this
    /// machine" into "media in the bubble": `mafold attach` and the agent render
    /// loop both come through here, so the daemon and the CLI can never drift on
    /// kind/id/mime/dimension handling.
    ///
    /// The wire's THREE media kinds are all reachable from here — `classify`
    /// picks by content, not by hope. Sending everything as `photo` (what this
    /// did before) meant a `.html` the agent wrote arrived as a broken image
    /// bubble, which is also why agents stopped believing they could send files.
    pub async fn attach_media(&self, message_id: &str, path: &std::path::Path) -> Result<Value> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let (kind, mime) = classify(&name, &bytes);
        // The upload's filename must AGREE with the bytes (the declared name is
        // what the registry row keeps); a screenshot handed over as `shot.txt`
        // should register as the image it is. A file keeps the caller's own
        // name — that name is what the bubble displays.
        let upload_name = match kind {
            "file" => name.clone(),
            _ => retype(&name, mime),
        };
        // uploadFile answers with the FILE itself (`{id, unique_id, mime, …}`,
        // file-id world — no urls). The attachment carries nothing but that id;
        // name/size/dimensions live on the registry row the server just wrote.
        let up = self.upload_media(bytes, &upload_name, mime).await?;
        let file_id = up["id"].as_str().context("uploadFile returned no id")?;
        // `id` is the attachment's identity for the server's de-dup, so key it
        // on the file — re-attaching the same upload is a no-op, not a twin.
        let att = json!({ "kind": kind, "id": file_id, "file": file_id });
        self.attach(message_id, json!([att.clone()])).await?;
        Ok(att)
    }

    /// Close the loop on a card tap: the API is holding the tapper's request open
    /// on this `action_id`. `result` is `{"kind":"patch","content":…}`,
    /// `{"kind":"error","message":…}` or `{"kind":"ok"}` — a patch is both
    /// returned to the tapper and written into the message by the server, so the
    /// daemon must NOT also edit it here.
    ///
    /// Not retried: the caller is parked on a timeout, so a slow retry answers an
    /// audience that already left.
    pub async fn answer_card_action(
        &self,
        action_id: &str,
        result: serde_json::Value,
    ) -> Result<()> {
        self.post(
            "answerCardAction",
            json!({ "action_id": action_id, "result": result }),
        )
        .await?;
        Ok(())
    }
    /// RETRIED: finalizing an already-finalized message is a no-op server-side,
    /// and an unfinalized draft is precisely the "forever generating" bubble.
    pub async fn finalize(&self, message_id: &str) -> Result<()> {
        self.post_idempotent("botFinalize", json!({ "message_id": message_id }))
            .await?;
        Ok(())
    }

    /// Throw away one of our own UNFINALIZED drafts — the row goes, with no
    /// tombstone. The server refuses anything already finalized, so this can
    /// never be used to erase a real message.
    ///
    /// Used when a turn is steered: the reply re-opens BELOW the message that
    /// steered it, and the bubble it was streaming into has to stop existing
    /// rather than linger above saying "deleted".
    pub async fn discard_draft(&self, message_id: &str) -> Result<()> {
        self.post_idempotent("discardDraft", json!({ "message_id": message_id }))
            .await?;
        Ok(())
    }

    /// Push a directed alert popup to a single user (Telegram answerCallbackQuery
    /// `show_alert` analog). Used to tell a non-allow-listed user their Stop
    /// request was denied. `level` ∈ {info, success, error}.
    pub async fn push_alert(
        &self,
        to: &str,
        title: Option<&str>,
        text: &str,
        level: &str,
    ) -> Result<()> {
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
        self.post(
            "answerInlineQuery",
            json!({ "query_id": query_id, "results": results }),
        )
        .await?;
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
            anyhow::bail!(
                "publishCard: {}",
                v["description"].as_str().unwrap_or("error")
            );
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
        self.post("unpublishCard", json!({ "tag": tag, "version": version }))
            .await
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
            anyhow::bail!(
                "publishApp: {}",
                v["description"].as_str().unwrap_or("error")
            );
        }
        Ok(v["result"].clone())
    }

    /// Upload a file via `/api/uploadFile`. Returns the bare `FileRef`
    /// (`{id, unique_id, mime, size_bytes, filename?, w?, h?}`) — NOT the
    /// `{ok,result}` envelope. The `id` is the handle everything else uses.
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
        self.post("resolveLangPacks", json!({ "requests": requests }))
            .await
    }
    /// The languages the server currently serves (newest version each).
    pub async fn list_languages(&self) -> Result<Value> {
        self.post("listLanguages", json!({})).await
    }

    fn ws_url(&self) -> String {
        let ws = self
            .base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{ws}/api/ws")
    }

    /// The WS handshake request — sends the bot token via an `Authorization:
    /// Bearer` header instead of the URL query string (so the secret doesn't sit
    /// in logs / proxies). Feed it to `ws_connect`.
    pub fn ws_request(&self) -> tokio_tungstenite::tungstenite::ClientRequestBuilder {
        let uri: tokio_tungstenite::tungstenite::http::Uri =
            self.ws_url().parse().expect("ws url should be a valid URI");
        tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {}", self.token))
    }

    /// Open the daemon's WebSocket — through the HTTP proxy the environment
    /// names, exactly like the HTTP half of this client already does.
    ///
    /// This exists because the two halves disagreed by default: `reqwest` reads
    /// `HTTPS_PROXY`/`NO_PROXY` on its own, while `tokio_tungstenite::connect_async`
    /// reads nothing and always dials the origin direct. Behind a proxy that is
    /// the ONLY route out (observed: `getMe` 200 through the proxy while every
    /// WS attempt died in SYN_SENT to the origin IP), that split makes a daemon
    /// that authenticates, publishes its command menu, and then can never
    /// receive a single message — online-looking and deaf. A tunnelled socket
    /// keeps the TLS peer and SNI at the origin host, so the token still only
    /// travels inside TLS and the proxy sees nothing but `CONNECT host:443`.
    ///
    /// Returns the same pair `connect_async` did, so callers keep matching on
    /// `tungstenite::Error::Http` for the 401/403 auth-rejection path.
    pub async fn ws_connect(
        &self,
    ) -> Result<
        (
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
            tokio_tungstenite::tungstenite::handshake::client::Response,
        ),
        tokio_tungstenite::tungstenite::Error,
    > {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::{error::UrlError, Error};

        let request = self.ws_request().into_client_request()?;
        let uri = request.uri().clone();
        let host = uri.host().ok_or(Error::Url(UrlError::NoHostName))?.to_string();
        let tls = uri.scheme_str() == Some("wss");
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

        let proxy = ws_proxy_for(&host, tls);
        // One line per process, not per reconnect: which route the socket takes
        // is the first thing worth knowing when a daemon won't come online, and
        // the reconnect loop would otherwise bury it.
        static NOTE: std::sync::Once = std::sync::Once::new();
        NOTE.call_once(|| match &proxy {
            Some(p) => println!("ws transport: {host}:{port} via proxy {}", redact_proxy(p)),
            None => println!("ws transport: {host}:{port} direct (no proxy in env)"),
        });

        let stream = match &proxy {
            Some(p) => ws_tunnel(p, &host, port).await?,
            None => tokio::net::TcpStream::connect((host.as_str(), port)).await.map_err(Error::Io)?,
        };
        // The handshake is one small write followed by a wait; Nagle would sit
        // on it for a round trip.
        let _ = stream.set_nodelay(true);

        // `request` still carries the wss:// URI, so this does the TLS handshake
        // against the ORIGIN (SNI + cert check on `host`) over whatever socket we
        // just handed it — proxied or not.
        tokio_tungstenite::client_async_tls_with_config(request, stream, None, None).await
    }
}

/// First non-empty value among `names`. Both cases are probed because only
/// Windows env lookup is case-insensitive; the lowercase spellings are the
/// conventional ones on Unix.
fn env_first(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

/// Whether `NO_PROXY` exempts `host`. Supports the three forms that actually
/// appear in the wild: `*` (everything), a `192.168.*` prefix wildcard, and a
/// bare or dot-led suffix (`mafold.com` / `.mafold.com`) which also matches
/// subdomains. Keeping this honest is what stops a local `127.0.0.1:4000` dev
/// server from being dialled through the proxy.
fn no_proxy_matches(host: &str) -> bool {
    let Some(list) = env_first(&["NO_PROXY", "no_proxy"]) else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    list.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .any(|pat| {
            if pat == "*" {
                return true;
            }
            if let Some(prefix) = pat.strip_suffix('*') {
                return !prefix.is_empty() && host.starts_with(prefix);
            }
            let base = pat.trim_start_matches('.');
            !base.is_empty() && (host == base || host.ends_with(&format!(".{base}")))
        })
}

/// The proxy to dial for a WS connection to `host`, or None for direct. Same
/// precedence reqwest applies on the HTTP side: `NO_PROXY` wins outright, then
/// the scheme-specific variable, then `ALL_PROXY`.
fn ws_proxy_for(host: &str, tls: bool) -> Option<String> {
    if no_proxy_matches(host) {
        return None;
    }
    let scheme_specific: &[&str] =
        if tls { &["HTTPS_PROXY", "https_proxy"] } else { &["HTTP_PROXY", "http_proxy"] };
    env_first(scheme_specific).or_else(|| env_first(&["ALL_PROXY", "all_proxy"]))
}

/// Credentials stripped — this string is printed, and proxy URLs carry
/// `user:pass@` often enough to matter.
fn redact_proxy(proxy: &str) -> String {
    let (scheme, rest) = proxy.split_once("://").unwrap_or(("http", proxy));
    match rest.rsplit_once('@') {
        Some((_, addr)) => format!("{scheme}://***@{addr}"),
        None => format!("{scheme}://{rest}"),
    }
}

/// A TCP socket to `host:port` tunnelled through an HTTP proxy via `CONNECT`.
///
/// SOCKS is deliberately not handled: `reqwest` here is built without its
/// `socks` feature, so the HTTP half ignores a `socks5://` value too — failing
/// loudly beats the two halves silently disagreeing again.
async fn ws_tunnel(
    proxy: &str,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, tokio_tungstenite::tungstenite::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let io_err = |msg: String| {
        tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
    };

    let (scheme, rest) = proxy.split_once("://").unwrap_or(("http", proxy));
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(io_err(format!(
            "unsupported proxy scheme `{scheme}` for the WebSocket — only http:// CONNECT is handled"
        )));
    }
    let rest = rest.trim_end_matches('/');
    let (credentials, addr) = match rest.rsplit_once('@') {
        Some((c, a)) => (Some(c.to_string()), a),
        None => (None, rest),
    };
    // Bare `host` with no port is legal in these variables; 80 is the http
    // default and what reqwest assumes too.
    let proxy_addr = if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        addr.to_string()
    } else {
        format!("{addr}:80")
    };

    let mut stream = tokio::net::TcpStream::connect(&proxy_addr)
        .await
        .map_err(|e| io_err(format!("proxy {proxy_addr} unreachable: {e}")))?;

    let target = format!("{host}:{port}");
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(credentials) = credentials {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
        req.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    stream.write_all(req.as_bytes()).await.map_err(tokio_tungstenite::tungstenite::Error::Io)?;

    // Byte at a time, stopping dead on the blank line: read any further and we
    // would swallow the head of the TLS handshake that follows on this socket.
    let mut head = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await.map_err(tokio_tungstenite::tungstenite::Error::Io)? {
            0 => return Err(io_err("proxy closed the connection during CONNECT".into())),
            _ => head.push(byte[0]),
        }
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 8192 {
            return Err(io_err("proxy sent an oversized CONNECT response".into()));
        }
    }

    let status_line = String::from_utf8_lossy(&head);
    let status_line = status_line.lines().next().unwrap_or_default().trim();
    let accepted = status_line.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2'));
    if !accepted {
        return Err(io_err(format!("proxy refused CONNECT {target}: {status_line}")));
    }
    Ok(stream)
}

/// What a local file becomes in the bubble: `(kind, mime)`, where `kind` is one
/// of the wire's media kinds (`photo` / `video` / `file`).
///
/// The BYTES decide, not the name. A file only rides the photo/video path if its
/// header proves it is one — everything else is a `file` card with an honest
/// mime, which is what the web composer produces for the same upload. That
/// asymmetry is the whole bug: a name-based guess turned every attachment into a
/// photo, so `.html`, `.pdf` and a mis-saved screenshot alike arrived as broken
/// image bubbles.
fn classify(name: &str, bytes: &[u8]) -> (&'static str, &'static str) {
    match sniff_media(bytes) {
        Some(m) if m.starts_with("image/") => ("photo", m),
        Some(m) => ("video", m),
        None => ("file", mime_from_ext(name)),
    }
}

/// Formats we can PROVE from the header and that the media pipeline serves as
/// playable/renderable media. Deliberately narrow: anything a sniff can't
/// confirm becomes a file card, which degrades to "a download that works"
/// instead of "an image that doesn't".
fn sniff_media(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // ISO-BMFF (`....ftyp<brand>`) covers HEIC, MP4 and MOV — same container,
    // different major brand, so the brand is what tells them apart.
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        // Unknown brand ⇒ None on purpose: `M4A `/`M4B ` are audio, which has no
        // media kind of its own, and a file card beats a silent video player.
        return match &b[8..12] {
            b"heic" | b"heix" | b"hevc" | b"heim" | b"mif1" | b"msf1" => Some("image/heic"),
            b"qt  " => Some("video/quicktime"),
            b"isom" | b"iso2" | b"iso4" | b"iso5" | b"mp41" | b"mp42" | b"avc1" | b"dash"
            | b"M4V " | b"mmp4" => Some("video/mp4"),
            _ => None,
        };
    }
    None
}

/// The same basename carrying the extension that matches `mime` — what we
/// upload media under once the header has overruled the caller's name.
fn retype(name: &str, mime: &str) -> String {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        _ => "png",
    };
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let stem = if stem.is_empty() { "file" } else { stem };
    format!("{stem}.{ext}")
}

/// Content type for everything else, by extension. Only a display/download hint
/// — the server's own allow-list still decides how the bytes are STORED (an
/// `.html` is kept extensionless there so it can never be served as an active
/// same-origin document).
fn mime_from_ext(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "csv" => "text/csv; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Intrinsic size from a PNG's IHDR, so the bubble can reserve the box before
/// the bytes arrive (no layout jump on load). Header-only — we never decode the
/// image. `None` for anything that isn't a PNG; the attachment is still valid
/// without `w`/`h`, the client just measures after load.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
    // 8 magic + 8 chunk header + 8 dimensions = the first 24 bytes.
    if bytes.len() < 24 || !bytes.starts_with(MAGIC) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let n = |o: usize| u32::from_be_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    Some((n(16), n(20)))
}

#[cfg(test)]
mod attach_tests {
    use super::{classify, png_dimensions, retype};

    /// A real PNG header: magic + IHDR length/type + 1122×1402 (the size the
    /// codex image that started all this actually came back at).
    fn png_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn png_size_comes_from_the_ihdr() {
        assert_eq!(png_dimensions(&png_header(1122, 1402)), Some((1122, 1402)));
    }

    /// No dimensions is a fine attachment (the client measures after load), so
    /// anything that isn't a PNG must decline rather than return garbage.
    #[test]
    fn non_png_bytes_have_no_dimensions() {
        assert_eq!(png_dimensions(b"\xff\xd8\xff\xe0 jpeg"), None);
        assert_eq!(png_dimensions(b""), None);
        assert_eq!(png_dimensions(&png_header(1, 1)[..12]), None); // truncated
        let mut wrong_chunk = png_header(4, 4);
        wrong_chunk[12..16].copy_from_slice(b"iTXt"); // valid PNG, not IHDR first
        assert_eq!(png_dimensions(&wrong_chunk), None);
    }

    /// `.....ftyp<brand>` — the ISO-BMFF header MP4/MOV/HEIC all share.
    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x18];
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(brand);
        v
    }

    /// Real images and clips keep riding the media path — proven by their
    /// header, so a screenshot saved under the wrong name still lands as a photo.
    #[test]
    fn header_decides_the_media_kind() {
        assert_eq!(classify("shot.png", &png_header(4, 4)), ("photo", "image/png"));
        assert_eq!(classify("shot.txt", &png_header(4, 4)), ("photo", "image/png"));
        assert_eq!(classify("a.jpg", b"\xff\xd8\xff\xe0JFIF"), ("photo", "image/jpeg"));
        assert_eq!(classify("a.gif", b"GIF89a\x01\0"), ("photo", "image/gif"));
        assert_eq!(classify("a.webp", b"RIFF\0\0\0\0WEBPVP8 "), ("photo", "image/webp"));
        assert_eq!(classify("a.heic", &ftyp(b"heic")), ("photo", "image/heic"));
        assert_eq!(classify("clip.mp4", &ftyp(b"isom")), ("video", "video/mp4"));
        assert_eq!(classify("clip.mov", &ftyp(b"qt  ")), ("video", "video/quicktime"));
    }

    /// THE BUG: an `.html` the agent wrote used to go out as `image/png` under
    /// `kind: photo`, so the bubble showed a broken image. It is a file card now,
    /// with the same mime the web composer sends for the same upload.
    #[test]
    fn documents_are_files_not_photos() {
        assert_eq!(
            classify("demo.html", b"<!doctype html><html>"),
            ("file", "text/html; charset=utf-8")
        );
        assert_eq!(classify("notes.md", b"# hi"), ("file", "text/markdown; charset=utf-8"));
        assert_eq!(classify("data.json", b"{}"), ("file", "application/json"));
        assert_eq!(classify("paper.pdf", b"%PDF-1.7"), ("file", "application/pdf"));
        assert_eq!(classify("logo.svg", b"<svg xmlns="), ("file", "image/svg+xml"));
        assert_eq!(classify("thing", b"\0\0binary"), ("file", "application/octet-stream"));
    }

    /// Media is stored under an extension that matches its bytes — the serve path
    /// types the file from that name, so `shot.txt` holding a PNG must not be
    /// handed to the server as text.
    #[test]
    fn media_uploads_under_the_extension_its_bytes_earn() {
        assert_eq!(retype("shot.txt", "image/png"), "shot.png");
        assert_eq!(retype("shot", "image/png"), "shot.png");
        assert_eq!(retype("clip.bin", "video/quicktime"), "clip.mov");
        assert_eq!(retype("a.b.jpeg", "image/jpeg"), "a.b.jpg");
        assert_eq!(retype(".hidden", "image/png"), "file.png");
    }

    /// A named-but-lying extension must not promote bytes onto the photo path:
    /// an HTML error page saved as `.png` is precisely the "broken image" report,
    /// and audio (ISO-BMFF, but no video brand) has no player of its own.
    #[test]
    fn a_lying_extension_cannot_fake_media() {
        assert_eq!(classify("broken.png", b"<html>404</html>"), ("file", "application/octet-stream"));
        assert_eq!(classify("song.m4a", &ftyp(b"M4A ")), ("file", "application/octet-stream"));
    }
}

/// Media origins the attachment downloader may fetch from, on top of `base`.
///
/// WHY THIS EXISTS: the allowlist used to be exactly "same origin as the api".
/// api@0.0.58 (2026-08-06) moved uploads to `cdn.mafold.com`, so `uploadFile`
/// started handing out absolute URLs on a DIFFERENT origin — and every image
/// sent to an agent from then on was refused right here. The caller only
/// `eprintln!`s the error, so the failure reached nobody: the picture simply
/// never arrived. (Older messages kept working: they store a relative
/// `/media/…`, which resolves onto the api and follows its redirect.)
///
/// Deliberately a LIST, not a `*.mafold.com` wildcard — a wildcard would make
/// every present and future subdomain, including anything an attacker might
/// get to host there, a legal SSRF target. Overridable so a self-hosted
/// deployment can name its own CDN.
fn extra_media_origins() -> Vec<String> {
    parse_media_origins(std::env::var("MAFOLD_MEDIA_ORIGINS").ok().as_deref())
}

/// Split out from the env read so it can be tested without mutating process
/// state (env is global; a test that sets it races every other test).
fn parse_media_origins(raw: Option<&str>) -> Vec<String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect(),
        None => vec!["https://cdn.mafold.com".to_string()],
    }
}

impl Client {
    /// Is this absolute URL one we're willing to fetch an attachment from?
    fn media_origin_allowed(&self, url: &str) -> bool {
        same_origin(&self.base, url)
            || extra_media_origins().iter().any(|o| same_origin(o, url))
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

#[cfg(test)]
mod media_origin_tests {
    use super::*;

    fn client(base: &str) -> Client {
        Client::new(base.to_string(), "t".into())
    }

    /// The regression this whole allowlist entry exists for: media moved to the
    /// CDN origin in api@0.0.58 and every agent image was refused for four days.
    #[test]
    fn cdn_origin_is_allowed_alongside_the_api() {
        let c = client("https://api.mafold.com");
        assert!(c.media_origin_allowed("https://cdn.mafold.com/abc.png"));
        assert!(c.media_origin_allowed("https://api.mafold.com/media/abc.png"));
    }

    /// A wildcard would have been the lazy fix; these are what it would let in.
    #[test]
    fn look_alike_and_foreign_hosts_are_refused() {
        let c = client("https://api.mafold.com");
        for bad in [
            "https://evil.com/x.png",
            "https://cdn.mafold.com.evil.com/x.png", // suffix trick
            "https://evil.com/cdn.mafold.com/x.png", // path trick
            "http://cdn.mafold.com/x.png",           // scheme downgrade
            "https://sub.cdn.mafold.com/x.png",      // not the named origin
        ] {
            assert!(!c.media_origin_allowed(bad), "should have refused {bad}");
        }
    }

    #[test]
    fn origins_are_configurable_for_self_hosted_deployments() {
        assert_eq!(parse_media_origins(None), vec!["https://cdn.mafold.com"]);
        assert_eq!(
            parse_media_origins(Some(" https://a.example/ , https://b.example ,, ")),
            vec!["https://a.example", "https://b.example"]
        );
        // Blank means UNSET, not "allow nothing" — same rule the api applies to
        // its keys (state.rs), because a stray `VAR=` in an env file is an
        // accident far more often than an intent, and here that accident would
        // silently stop every attachment from loading.
        assert_eq!(parse_media_origins(Some("   ")), vec!["https://cdn.mafold.com"]);
    }
}

/// The "a turn must never vanish" contract: what a failing `botCreateDraft`
/// does, and what the user is told when it fails for good.
#[cfg(test)]
mod lost_turn_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A single-file HTTP stub: answers every request with `status`/`reply` and
    /// records each (path, body) it saw. Enough server for the real transport —
    /// retries and all — to be exercised without a network or a mock crate.
    async fn stub(status: u16, reply: &'static str) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read headers, then exactly Content-Length bytes of body.
                    let (path, body) = loop {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let Some(end) = text.find("\r\n\r\n") else { continue };
                        let head = text[..end].to_string();
                        let want: usize = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let body = text[end + 4..].to_string();
                        if body.len() < want {
                            continue;
                        }
                        let path = head
                            .lines()
                            .next()
                            .and_then(|l| l.split(' ').nth(1))
                            .unwrap_or("")
                            .to_string();
                        break (path, body);
                    };
                    log.lock().unwrap().push((path, body));
                    let out = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[test]
    fn a_blip_is_the_far_end_falling_over_not_the_far_end_saying_no() {
        use mafold_core::RpcError as R;
        // It broke inside the handler → the same request may well work now.
        assert!(Client::is_server_blip(&R::Api(
            r#"{"ok":false,"error_code":500,"description":"boom"}"#.into()
        )));
        assert!(Client::is_server_blip(&R::Transport(
            "botCreateDraft: HTTP 502 — <html>bad gateway</html>".into()
        )));
        assert!(Client::is_server_blip(&R::Transport(
            "error decoding response body".into()
        )));
        // A verdict, a version gap, and connect (retried on its own terms).
        assert!(!Client::is_server_blip(&R::Api(
            r#"{"ok":false,"error_code":403,"description":"not authorized"}"#.into()
        )));
        assert!(!Client::is_server_blip(&R::Transport(
            "getUser: HTTP 404 — this server has no such method".into()
        )));
        assert!(!Client::is_server_blip(&R::UnknownMethod("nope".into())));
        assert!(!Client::is_server_blip(&R::Connect("refused".into())));
    }

    /// A 5xx gets one more shot — the blip that ate a message on 2026-08-18 —
    /// but only one: the call is not idempotent, so hammering it would trade a
    /// lost message for a row of empty bubbles.
    #[tokio::test]
    async fn a_5xx_create_draft_is_retried_exactly_once() {
        let (base, seen) = stub(500, r#"{"ok":false,"error_code":500,"description":"boom"}"#).await;
        let c = Client::new(base, "t".into());
        let err = c
            .create_draft("11111111-2222-3333-4444-555555555555", None, None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("botCreateDraft failed"), "{err:#}");
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 2, "expected one retry, got {calls:?}");
        assert!(calls[0].0.ends_with("/api/botCreateDraft"), "{:?}", calls[0].0);
    }

    /// …and when it does fail for good, the user hears about it ON the surface
    /// they asked from — channel and thread included, because a notice that
    /// lands in `#all` is a notice they never see.
    #[tokio::test]
    async fn the_notice_lands_where_the_message_was_asked() {
        let (base, seen) = stub(200, r#"{"ok":true,"result":{}}"#).await;
        let c = Client::new(base, "t".into());
        c.announce_lost_turn(
            Dest::chat("conv-1").channel(Some("chan-2")).thread(Some("root-3")),
            "botCreateDraft failed: HTTP 500 — boom",
        )
        .await
        .unwrap();
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.ends_with("/api/sendMessage"), "{:?}", calls[0].0);
        let body: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(body["chat_id"], "conv-1");
        assert_eq!(body["channel_id"], "chan-2");
        assert_eq!(body["thread_root_id"], "root-3");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("send it again"), "must say what to do: {text}");
        assert!(text.contains("HTTP 500"), "must keep the reason: {text}");
    }
}
