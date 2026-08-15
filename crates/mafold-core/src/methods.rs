//! The typed method layer — ONE definition of the API surface, in the core.
//!
//! Two tiers:
//! - `KNOWN_METHODS` + `ApiClient::call_raw`: the FULL route table (snapshotted
//!   from `mafold-api/src/main.rs`, the routing SSOT). Every client tunnels
//!   through here, so a method-name typo dies at the core boundary with
//!   `RpcError::UnknownMethod` instead of a confusing server 404. The
//!   `registry_matches_api_route_table` test pins this list to the api source.
//! - Typed wrappers (`get_me` → `wire::Account`, …): the hot chat subset with
//!   `mafold_types` params/results, so Rust consumers (mafold-cli today, the
//!   native sync engine next) get compile-time shapes. Wrappers are added as
//!   consumers need them; everything else goes through `call_raw`.
//!
//! `base` includes the `/api` prefix (e.g. `https://api.mafold.com/api`) —
//! the same convention as `net::rpc`.

use mafold_types as wire;
use serde::Serialize;
use uuid::Uuid;

use crate::net::{self, RpcError};

/// Every REST method the api serves (`.route("/api/<name>", …)` in
/// `mafold-api/src/main.rs`). Sorted; slashes are sub-path methods (auth/oauth/
/// moments). `ws` is the websocket upgrade — listed for completeness, never
/// POSTed through here.
pub static KNOWN_METHODS: &[&str] = &[
    "addChatMembers",
    "addGroupBot",
    "answerCardAction",
    "answerConnectionCall",
    "answerConnectionCallChunk",
    "answerInlineQuery",
    "appReleasePublish",
    "appRoom.get",
    "appRoom.increment",
    "appRoom.set",
    "appRoster",
    "appUpdateCheck",
    "appUpdateDownload",
    "approveVaultDevice",
    "auth/device/approve",
    "auth/device/poll",
    "auth/device/start",
    "auth/login",
    "auth/register",
    "blockUser",
    "botAppendDelta",
    "botAttach",
    "botCreateDraft",
    "botEditDraft",
    "botFinalize",
    "callConnection",
    "claimConnectionCall",
    "claimProvisions",
    "createBot",
    "createCapsule",
    "createChannel",
    "createChatInviteLink",
    "createGroup",
    "createToken",
    "debug.send_file_as_mafold",
    "debug.send_text_as_mafold",
    "deleteAccount",
    "deleteBot",
    "deleteCapsule",
    "deleteChannel",
    "deleteConnection",
    "deleteFolder",
    "deleteMessages",
    "deleteMessagesForMe",
    "deploySite",
    "disbandGroup",
    "editChannel",
    "editMessage",
    "ensureSelfConversation",
    "exchangeConnectionToken",
    "forwardMerged",
    "forwardMessages",
    "getAppConvConfig",
    "getAppLaunch",
    "getAppearancePrefs",
    "getBlockedUsers",
    "getBot",
    "getBotApps",
    "getBotCommands",
    "getBotConfig",
    "getBotConvConfig",
    "getBotToken",
    "getCapsules",
    "getChat",
    "getChatHistory",
    "getChatInviteInfo",
    "getChats",
    "getConnectionLink",
    "getConnectionProviders",
    "getContacts",
    "getDrafts",
    "getFlags",
    "getFolders",
    "getGroupBots",
    "getLangPackDiff",
    "getMe",
    "getMyBotConfig",
    "getMyBots",
    "getNotificationPrefs",
    "getOfficialBots",
    "getProfileFields",
    "getSessions",
    "getTemplates",
    "getThreadMessages",
    "getUpdates",
    "getUser",
    "getUserStatus",
    "getUsers",
    "getVaultKey",
    "getVaultRecovery",
    "githubWebhook",
    "installApp",
    "joinChatByInviteLink",
    "leaveChat",
    "listApps",
    "listBuilderGrants",
    "listCards",
    "listChannels",
    "listChatInviteLinks",
    "listComposerCards",
    "listConnections",
    "listFlags",
    "listHarnessHosts",
    "listInstalls",
    "listLanguages",
    "listOAuthGrants",
    "listSites",
    "listTokens",
    "listVaultDevices",
    "markChannelRead",
    "markRead",
    "markThreadRead",
    "moments/commentCreate",
    "moments/commentDelete",
    "moments/commentsList",
    "moments/likeSet",
    "moments/postGet",
    "moments/postUpsert",
    "moments/postsList",
    "moments/postsListMine",
    "moments/publicationGet",
    "moments/saveSet",
    "moments/savedList",
    "moments/sitemapList",
    "moments/tagDelete",
    "moments/tagUpsert",
    "moments/tagsList",
    "moments/timelineList",
    "oauth/authorize",
    "oauth/check",
    "oauth/token",
    "oauth/userinfo",
    "pinApp",
    "pinChat",
    "pinMessage",
    "publishApp",
    "publishCard",
    "publishConnectionProviders",
    "publishLangPack",
    "pushAlert",
    "putConnection",
    "putVaultRecovery",
    "queryInline",
    "registerPushToken",
    "registerVaultDevice",
    "registerWebApp",
    "removeApp",
    "removeBotKey",
    "removeChatMember",
    "removeGroupBot",
    "removeSite",
    "renameChannel",
    "reorderFolders",
    "reportConnectionLink",
    "reportHarnesses",
    "reportMessage",
    "reportUser",
    "requestBuilderGrant",
    "requestConnectionAccess",
    "requestRuntimeInstall",
    "resolveApps",
    "resolveCards",
    "resolveLangPacks",
    "revokeBotToken",
    "revokeBuilderGrant",
    "revokeChatInviteLink",
    "revokeOAuthGrant",
    "revokeToken",
    "revokeVaultDevice",
    "roomChange",
    "roomChanges",
    "rotateAppSecret",
    "saveDraft",
    "saveFolder",
    "searchMessages",
    "searchUsers",
    "sendChatAction",
    "sendComponentAction",
    "sendMessage",
    "setAppConvConfig",
    "setAppearancePrefs",
    "setAutoTerminate",
    "setBotCommands",
    "setBotConfig",
    "setBotConvConfig",
    "setBotKey",
    "setChannelArchived",
    "setChannelClosed",
    "setChannelNotifications",
    "setChannelPinned",
    "setChatDescription",
    "setChatNotifications",
    "setChatPermissions",
    "setChatPhoto",
    "setChatTitle",
    "setDeviceNotificationPrefs",
    "setFlag",
    "setForumEnabled",
    "setForumPermissions",
    "setGroupAdmin",
    "setGroupBotMode",
    "setMessageReaction",
    "setMyBotConfig",
    "setNotificationPrefs",
    "signIn",
    "startChat",
    "startConnectionLink",
    "stopRun",
    "terminateOtherSessions",
    "terminateSession",
    "transferGroupOwner",
    "unblockUser",
    "uninstallApp",
    "unpinApp",
    "unpinChat",
    "unpinMessage",
    "unpublishCard",
    "updateProfile",
    "uploadFile",
    "walletBalances",
    "walletConvert",
    "walletDebit",
    "walletGrantRevoke",
    "walletGrantSet",
    "walletGrants",
    "walletHistory",
    "walletMint",
    "walletPacketClaim",
    "walletPacketCreate",
    "walletPacketGet",
    "walletRates",
    "walletRatesSet",
    "walletTransfer",
    "walletTransferAccept",
    "walletTransferDecline",
    "walletTransferOffer",
    "ws",
];

/// Is `method` in the api's route table? (Binary search — the list is sorted.)
pub fn is_known(method: &str) -> bool {
    KNOWN_METHODS.binary_search(&method).is_ok()
}

/// Validated generic call: reject unknown method names BEFORE any I/O, then
/// tunnel through `net::rpc_ex`. This is the single transport every surface
/// funnels into (cli, wasm `rpc`, native `call`).
pub async fn call(base: &str, token: &str, method: &str, body: &str) -> Result<String, RpcError> {
    if !is_known(method) {
        return Err(RpcError::UnknownMethod(method.to_string()));
    }
    net::rpc_ex(base, token, method, body).await
}

/// Params for `sendMessage` — mirrors the api's `SendMessageBody` (bot_api.rs).
#[derive(Debug, Default, Clone, Serialize)]
pub struct SendMessageParams {
    pub chat_id: Uuid,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<Uuid>,
    /// Slack-style thread reply under this root (server normalizes to the root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<Uuid>,
    /// Forum channel to post into. None = the `#all` main timeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<wire::Attachment>,
    /// Echoed back on events.messageNew for optimistic-send reconcile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
}

/// A typed handle on the Mafold REST api: `base` (with `/api`) + Bearer token.
/// Cheap to construct; the underlying HTTP client/pool is shared process-wide.
#[derive(Clone)]
pub struct ApiClient {
    pub base: String,
    pub token: String,
}

impl ApiClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self { base: base.into(), token: token.into() }
    }

    /// Validated, untyped escape hatch — for the long tail of methods that
    /// don't have a typed wrapper (yet). `body_json` must be a JSON object.
    pub async fn call_raw(&self, method: &str, body_json: &str) -> Result<String, RpcError> {
        call(&self.base, &self.token, method, body_json).await
    }

    /// Serialize params → call → deserialize the typed result.
    async fn call_typed<P: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, RpcError> {
        let body = serde_json::to_string(params)
            .map_err(|e| RpcError::Transport(format!("params encode: {e}")))?;
        let result = self.call_raw(method, &body).await?;
        serde_json::from_str(&result)
            .map_err(|e| RpcError::Transport(format!("{method} result decode: {e}")))
    }

    // ── accounts ──
    pub async fn get_me(&self) -> Result<wire::Account, RpcError> {
        self.call_typed("getMe", &serde_json::json!({})).await
    }
    /// NB the api's field is `user_id` (it accepts a username string there).
    pub async fn get_user(&self, username: &str) -> Result<wire::Account, RpcError> {
        self.call_typed("getUser", &serde_json::json!({ "user_id": username })).await
    }

    // ── conversations ──
    pub async fn get_chats(&self) -> Result<wire::ConversationsPage, RpcError> {
        self.call_typed("getChats", &serde_json::json!({})).await
    }
    pub async fn get_chat(&self, chat_id: Uuid) -> Result<wire::Conversation, RpcError> {
        self.call_typed("getChat", &serde_json::json!({ "chat_id": chat_id })).await
    }
    pub async fn start_chat(&self, user_ids: &[&str]) -> Result<wire::Conversation, RpcError> {
        self.call_typed("startChat", &serde_json::json!({ "user_ids": user_ids })).await
    }

    // ── history ──
    pub async fn get_chat_history(
        &self,
        chat_id: Uuid,
        limit: usize,
        channel_id: Option<Uuid>,
    ) -> Result<wire::MessagesPage, RpcError> {
        let mut body = serde_json::json!({ "chat_id": chat_id, "limit": limit });
        if let Some(ch) = channel_id {
            body["channel_id"] = serde_json::json!(ch);
        }
        self.call_typed("getChatHistory", &body).await
    }
    pub async fn get_thread_messages(
        &self,
        chat_id: Uuid,
        root_message_id: Uuid,
        limit: usize,
    ) -> Result<wire::MessagesPage, RpcError> {
        self.call_typed(
            "getThreadMessages",
            &serde_json::json!({ "chat_id": chat_id, "root_message_id": root_message_id, "limit": limit }),
        )
        .await
    }

    // ── send ──
    pub async fn send_message(&self, params: &SendMessageParams) -> Result<wire::Message, RpcError> {
        self.call_typed("sendMessage", params).await
    }

    // ── forum channels ──
    pub async fn list_channels(&self, chat_id: Uuid) -> Result<Vec<wire::Channel>, RpcError> {
        self.call_typed("listChannels", &serde_json::json!({ "chat_id": chat_id })).await
    }

    // ── bot streaming-draft pipeline (the daemon's reply path) ──
    /// Open a streaming draft; returns the draft `Message` (its id is what
    /// `bot_append_delta`/`bot_finalize` target).
    pub async fn bot_create_draft(
        &self,
        chat_id: Uuid,
        thread_root_id: Option<Uuid>,
        channel_id: Option<Uuid>,
    ) -> Result<wire::Message, RpcError> {
        let mut body = serde_json::json!({ "chat_id": chat_id });
        if let Some(root) = thread_root_id {
            body["thread_root_id"] = serde_json::json!(root);
        }
        if let Some(ch) = channel_id {
            body["channel_id"] = serde_json::json!(ch);
        }
        self.call_typed("botCreateDraft", &body).await
    }
    pub async fn bot_append_delta(&self, message_id: Uuid, delta: &str) -> Result<(), RpcError> {
        self.call_raw(
            "botAppendDelta",
            &serde_json::json!({ "message_id": message_id, "delta": delta }).to_string(),
        )
        .await
        .map(|_| ())
    }
    pub async fn bot_finalize(&self, message_id: Uuid) -> Result<(), RpcError> {
        self.call_raw("botFinalize", &serde_json::json!({ "message_id": message_id }).to_string())
            .await
            .map(|_| ())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn registry_is_sorted_and_validates() {
        let mut sorted = KNOWN_METHODS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KNOWN_METHODS, "KNOWN_METHODS must stay sorted (binary_search)");
        assert!(is_known("sendMessage"));
        assert!(is_known("auth/login"));
        assert!(!is_known("sendMesage")); // the typo class this layer exists to catch
        assert!(!is_known(""));
    }

    // Pin the registry to the api's route table (the routing SSOT). Reads the
    // api source at TEST time only — runs in-repo/CI, never in the api's Docker
    // build (which can't see this crate anyway).
    #[test]
    fn registry_matches_api_route_table() {
        let src = include_str!("../../mafold-api/src/main.rs");
        let mut routed: Vec<&str> = src
            .match_indices(".route(\"/api/")
            .map(|(i, _)| {
                let rest = &src[i + ".route(\"/api/".len()..];
                &rest[..rest.find('"').expect("unterminated route path")]
            })
            .collect();
        routed.sort_unstable();
        routed.dedup();
        let known: Vec<&str> = KNOWN_METHODS.to_vec();
        assert_eq!(
            known, routed,
            "core methods registry drifted from mafold-api/src/main.rs — update KNOWN_METHODS"
        );
    }

    #[tokio::test]
    async fn unknown_method_rejected_before_io() {
        // Base that would explode if dialed — proves validation happens first.
        let e = call("http://127.0.0.1:1", "t", "sendMesage", "{}").await.unwrap_err();
        assert!(matches!(e, RpcError::UnknownMethod(_)));
    }

    // ── LIVE smoke (needs a local in-memory api on :4000) ──
    // Run: cargo run -p mafold-api (no DATABASE_URL) · then
    //      cargo test -p mafold-core --features(none) -- --ignored live_
    #[tokio::test]
    #[ignore]
    async fn live_typed_roundtrip() {
        let api = ApiClient::new("http://127.0.0.1:4000/api", "dev:ops");
        let me = api.get_me().await.expect("getMe");
        assert_eq!(me.username, "ops");

        let peer = api.get_user("eons").await.expect("getUser");
        let conv = api.start_chat(&[&peer.username]).await.expect("startChat");

        let sent = api
            .send_message(&SendMessageParams {
                chat_id: conv.id,
                text: "typed-methods live smoke".into(),
                client_msg_id: Some("client_live_smoke_1".into()),
                ..Default::default()
            })
            .await
            .expect("sendMessage");
        assert_eq!(sent.content, "typed-methods live smoke");
        assert_eq!(sent.client_msg_id.as_deref(), Some("client_live_smoke_1"));

        let page = api.get_chat_history(conv.id, 10, None).await.expect("history");
        assert!(page.items.iter().any(|m| m.id == sent.id), "sent message in history");

        // Draft pipeline (ordinary account works — accounts are symmetric).
        let draft = api.bot_create_draft(conv.id, None, None).await.expect("draft");
        api.bot_append_delta(draft.id, "hello ").await.expect("delta1");
        api.bot_append_delta(draft.id, "world").await.expect("delta2");
        api.bot_finalize(draft.id).await.expect("finalize");
        let page = api.get_chat_history(conv.id, 10, None).await.expect("history2");
        let done = page.items.iter().find(|m| m.id == draft.id).expect("draft in history");
        assert_eq!(done.content, "hello world");
        assert!(done.finalized_at.is_some());

        // Unknown method still dies at the boundary with a live server up.
        let e = api.call_raw("sendMesage", "{}").await.unwrap_err();
        assert!(matches!(e, RpcError::UnknownMethod(_)));
    }

    // ── typed layer vs a MOCK server (offline; the wire-shape contracts) ──
    use crate::testutil::{ok, spawn_mock};

    const ACCOUNT_JSON: &str = r#"{"username":"ops","display_name":"Ops","kind":"human"}"#;

    #[tokio::test]
    async fn typed_get_me_decodes_account() {
        let mock = spawn_mock(vec![ok(ACCOUNT_JSON)]);
        let api = ApiClient::new(&mock.base, "tok-123");
        let me = api.get_me().await.expect("getMe");
        assert_eq!(me.username, "ops");
        // The request really carried the method path + bearer token.
        let req = mock.request(0);
        assert_eq!(req.path, "/getMe");
        assert_eq!(req.auth.as_deref(), Some("Bearer tok-123"));
    }

    /// The `skip_serializing_if` contract the server relies on: absent options
    /// must be ABSENT KEYS (a JSON null where the server expects a Uuid would
    /// 422), and set options must appear.
    #[tokio::test]
    async fn typed_send_message_serializes_params() {
        let msg_json = format!(
            r#"{{"id":"6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5","conversation_id":"0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1","sender":{ACCOUNT_JSON},"content":"hi","created_at":"2026-07-14T00:00:00Z","reactions":[]}}"#,
        );
        let mock = spawn_mock(vec![ok(&msg_json), ok(&msg_json)]);
        let api = ApiClient::new(&mock.base, "t");
        let chat = Uuid::parse_str("0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1").unwrap();

        // Minimal params: every optional key must be absent.
        api.send_message(&SendMessageParams { chat_id: chat, text: "hi".into(), ..Default::default() })
            .await
            .expect("send minimal");
        let body: serde_json::Value = serde_json::from_str(&mock.request(0).body).unwrap();
        let obj = body.as_object().unwrap();
        for absent in ["reply_to_message_id", "thread_root_id", "channel_id", "attachments", "client_msg_id"] {
            assert!(!obj.contains_key(absent), "{absent} must be an ABSENT key when unset");
        }
        assert_eq!(obj["text"], "hi");

        // Full params: the set options must appear.
        let ch = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        api.send_message(&SendMessageParams {
            chat_id: chat,
            text: "hi".into(),
            channel_id: Some(ch),
            client_msg_id: Some("client_x".into()),
            ..Default::default()
        })
        .await
        .expect("send full");
        let body: serde_json::Value = serde_json::from_str(&mock.request(1).body).unwrap();
        assert_eq!(body["channel_id"], ch.to_string());
        assert_eq!(body["client_msg_id"], "client_x");
    }

    #[tokio::test]
    async fn typed_result_decode_error_is_transport() {
        let mock = spawn_mock(vec![ok(r#"{"nope":1}"#)]);
        let api = ApiClient::new(&mock.base, "t");
        match api.get_me().await.unwrap_err() {
            RpcError::Transport(m) => assert!(m.starts_with("getMe result decode: "), "{m}"),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn typed_api_error_passes_envelope() {
        let raw = r#"{"ok":false,"error_code":403,"description":"forbidden"}"#;
        let mock = spawn_mock(vec![(200, raw.into())]);
        let api = ApiClient::new(&mock.base, "t");
        match api.get_me().await.unwrap_err() {
            RpcError::Api(env) => assert_eq!(env, raw),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn typed_get_chat_history_channel_param() {
        let page = r#"{"items":[]}"#;
        let mock = spawn_mock(vec![ok(page), ok(page)]);
        let api = ApiClient::new(&mock.base, "t");
        let chat = Uuid::parse_str("0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1").unwrap();
        let ch = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

        api.get_chat_history(chat, 50, None).await.expect("main");
        let body: serde_json::Value = serde_json::from_str(&mock.request(0).body).unwrap();
        assert!(!body.as_object().unwrap().contains_key("channel_id"));
        assert_eq!(body["limit"], 50);

        api.get_chat_history(chat, 50, Some(ch)).await.expect("channel");
        let body: serde_json::Value = serde_json::from_str(&mock.request(1).body).unwrap();
        assert_eq!(body["channel_id"], ch.to_string());
    }

    /// The daemon's streaming pipeline wrappers: draft returns the typed
    /// Message; delta/finalize map any ok result to ().
    #[tokio::test]
    async fn bot_draft_wrappers_roundtrip() {
        let draft_json = format!(
            r#"{{"id":"6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5","conversation_id":"0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1","sender":{ACCOUNT_JSON},"content":"","created_at":"2026-07-14T00:00:00Z","reactions":[]}}"#,
        );
        let mock = spawn_mock(vec![ok(&draft_json), ok("{}"), ok("{}")]);
        let api = ApiClient::new(&mock.base, "t");
        let chat = Uuid::parse_str("0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1").unwrap();
        let ch = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

        let draft = api.bot_create_draft(chat, None, Some(ch)).await.expect("draft");
        assert_eq!(draft.id.to_string(), "6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5");
        let body: serde_json::Value = serde_json::from_str(&mock.request(0).body).unwrap();
        assert_eq!(body["channel_id"], ch.to_string());
        assert!(!body.as_object().unwrap().contains_key("thread_root_id"));

        api.bot_append_delta(draft.id, "hello").await.expect("delta");
        api.bot_finalize(draft.id).await.expect("finalize");
        assert_eq!(mock.request(1).path, "/botAppendDelta");
        assert_eq!(mock.request(2).path, "/botFinalize");
    }
}
