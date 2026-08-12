//! Mafold client core — the local-first single source of truth.
//!
//! DECOUPLED: portable async logic (`store::Store`) over a `storage::Storage` KV
//! trait. native = SQLite KV, exposed to Swift/Kotlin via UniFFI as a SYNCHRONOUS
//! surface (a tiny `block_on` over the always-ready native futures, so callers are
//! unchanged); web = wasm-bindgen (async → JS Promises) over IndexedDB (next).
//! FFI-friendly shapes (String ids, i64 epoch-ms) keep the boundary portable.

#[cfg(not(target_arch = "wasm32"))]
uniffi::setup_scaffolding!();

mod storage;
mod store;
pub mod net;
mod ws;
mod wire;
mod i18n;
pub mod flags;
pub mod connections;
pub mod mcp;
pub mod methods;
pub mod vault;
#[cfg(not(target_arch = "wasm32"))]
mod room;
/// Shared test helper (zero-dependency mock HTTP server) — compiled only under
/// `cargo test` on native; used by the net/methods/store test modules.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod testutil;

/// Test/bench-only seam — NOT public API. Benches (`benches/`) and integration
/// tests (`tests/wasm.rs`) compile as EXTERNAL crates, so the internals they
/// exercise must be reachable by path; re-exporting here beats making the
/// modules genuinely public.
#[doc(hidden)]
pub mod internal {
    pub use crate::i18n::LangPack;
    pub use crate::storage::Storage;
    pub use crate::store::Store;
    #[cfg(not(target_arch = "wasm32"))]
    pub use crate::storage::SqliteStore;
    #[cfg(target_arch = "wasm32")]
    pub use crate::storage::{IdbStore, MemStore};
}

/// Re-export the shared wire model so Rust consumers (mafold-cli) get the
/// protocol types from ONE dependency — no separate mafold-types import.
pub use mafold_types;
pub use net::RpcError;

#[derive(Debug, thiserror::Error)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Error))]
pub enum CoreError {
    #[error("db error: {0}")]
    Db(String),
}
#[cfg(not(target_arch = "wasm32"))]
impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self { CoreError::Db(e.to_string()) }
}

// ───────────────────────── data model ─────────────────────────
// Mirrors the wire shapes; serde for KV (de)serialization, uniffi::Record on
// native for the FFI boundary.

/// An asset, mirroring `mafold_types::FileRef`. Bytes come from
/// `<file_base>/<id>` — there is no url on the wire and none in the core; see
/// `.docs/file-id-v1.md`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreFileRef {
    pub id: String,
    #[serde(default)]
    pub unique_id: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub w: Option<u32>,
    #[serde(default)]
    pub h: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u32>,
    #[serde(default)]
    pub filename: Option<String>,
    /// Bytes are on the api host, not the CDN — append `id` to the api origin.
    #[serde(default)]
    pub local: bool,
}

impl From<&mafold_types::FileRef> for CoreFileRef {
    fn from(f: &mafold_types::FileRef) -> Self {
        Self {
            id: f.id.clone(),
            unique_id: f.unique_id.clone(),
            mime: f.mime.clone(),
            size_bytes: f.size_bytes,
            w: f.w,
            h: f.h,
            duration_ms: f.duration_ms,
            filename: f.filename.clone(),
            local: f.local,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreAccount {
    pub username: String,
    pub display_name: String,
    /// "human" | "bot".
    pub kind: String,
    #[serde(default, alias = "avatar_url")]
    pub avatar: Option<CoreFileRef>,
    /// Owner's username for a bot fork (`ops:claude` → `ops`).
    pub parent_username: Option<String>,
    /// Provider/template id for a bot (`claude` / `deepseek` / `claude-code`).
    pub template: Option<String>,
    /// Preferred UI language (BCP-47, e.g. "zh-Hans"). A cloud setting that syncs
    /// across devices; None ⇒ the client uses the device locale. See .docs/i18n-v0.md.
    pub language: Option<String>,
    /// Platform verification (blue check) — ops-granted; clients render a badge
    /// next to the display name. serde(default) so cached/older JSON (and web
    /// mappings) without the field still deserialize; the uniffi default keeps
    /// generated native constructors source-compatible (forum-flags dance).
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub verified: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreMessage {
    pub id: String,
    pub conversation_id: String,
    pub sender: CoreAccount,
    pub content: String,
    pub created_at_ms: i64,
    /// None while a streaming message is still being written.
    pub finalized_at_ms: Option<i64>,
    /// Echoed client id for optimistic-send reconciliation.
    pub client_msg_id: Option<String>,
    /// The id of the message this is a threaded reply to (Slack-style threads).
    /// `None` ⇒ a top-level channel message. Promoted out of `payload` so the
    /// store can split the channel timeline (`messages`) from a thread view
    /// (`thread`) without parsing the opaque blob. `#[serde(default)]` so caches
    /// written before this field still deserialize (they read back as top-level
    /// until the next upsert refreshes them).
    #[serde(default)]
    pub thread_root_id: Option<String>,
    /// Forum channel this message belongs to. `None` = the `#all` main timeline.
    /// Like `thread_root_id`, promoted out of `payload` so the store buckets
    /// channel timelines (`msg_key` keys on it) without parsing the blob.
    /// serde(default) keeps pre-field caches readable; the uniffi default keeps
    /// generated Swift/C# constructors source-compatible for existing callers.
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = None))]
    pub channel_id: Option<String>,
    /// Opaque host payload — the full client-side message record (attachments,
    /// reactions, reply, …) as JSON, stored verbatim so the chat view rehydrates
    /// faithfully; the structured fields above drive queries.
    pub payload: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreConversation {
    pub id: String,
    /// "direct" | "group".
    pub kind: String,
    pub title: Option<String>,
    pub participants: Vec<CoreAccount>,
    pub updated_at_ms: i64,
    pub unread_count: u32,
    /// Derived from the store — the dialog preview can never be stale or
    /// "No messages yet" while messages exist.
    pub last_message: Option<CoreMessage>,
    /// Forum (Telegram Topics) flags — carried through the cache so the header
    /// pill + channel management survive a reload. serde-default → old cached
    /// rows deserialize fine; uniffi defaults keep generated Swift/C#
    /// constructors source-compatible for callers that predate forums.
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub is_forum: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub forum_member_channels: bool,
    /// The other three member permissions (`Conversation.member_perms` on the
    /// wire; `forum_member_channels` above is the fourth, under its legacy
    /// name). Flat here because every other flag in this cache shape is —
    /// carried through for the same reason the forum flags are: a permission
    /// the cache drops is a button that flickers wrong after every reload.
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub member_add_members: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub member_edit_info: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub member_add_bots: bool,
}

/// A forum channel, cached so a conversation opens with its channel list
/// already drawn instead of blank-until-the-network-answers — the same
/// local-first contract messages and the dialog list already have.
///
/// Deliberately the SERVER's shape minus the fields no client renders from
/// cache: the cache exists to paint instantly, and the refresh right behind it
/// carries the authoritative row.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreChannel {
    pub id: String,
    pub conversation_id: String,
    pub name: String,
    pub order: i32,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = None))]
    pub icon: Option<String>,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = 0))]
    pub unread_count: u32,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub closed: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub pinned: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = false))]
    pub archived: bool,
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = None))]
    pub created_by: Option<String>,
    /// When this channel last saw activity — its newest message, or its own
    /// creation if nobody has posted yet. The picker sorts by this (a just-made
    /// channel is NEW, not stale), so the cache has to carry it or a cached
    /// list would come back in a different order than a fresh one.
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = 0))]
    pub activity_at_ms: i64,
}

/// A mini-app installed into a conversation, cached for the same reason: the
/// launcher should be there on open, not one network round-trip later.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Record))]
pub struct CoreConvApp {
    /// Namespaced app id (`owner/slug`) — the cache key.
    pub app_id: String,
    /// Sort position as the server listed it. Pinning is a server concept; the
    /// cache only preserves the order it was given.
    #[serde(default)]
    #[cfg_attr(not(target_arch = "wasm32"), uniffi(default = 0))]
    pub order: i32,
    /// The install entry VERBATIM, as the server returned it.
    ///
    /// The core deliberately does NOT model app manifests: capabilities, panel
    /// size, runtime and the rest evolve on the app platform's schedule, and a
    /// cache that re-declared them would silently drop every field it didn't
    /// know about. Each client parses this with the same code it uses for a
    /// live response, so cached and fresh apps can't diverge.
    pub entry_json: String,
}

/// Smoke test exposed to the host — the core crate version.
#[cfg_attr(not(target_arch = "wasm32"), uniffi::export)]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Sync engine stage 1 (native): the core makes the API call itself — async over
/// UniFFI (Swift `await`), POST {base}/{method} with a Bearer token. The web has
/// the same via the wasm `rpc` export. Since the typed-methods layer landed this
/// also VALIDATES the method name against the api route table (`methods`), so a
/// typo dies at the core boundary instead of a server 404.
#[cfg(not(target_arch = "wasm32"))]
#[uniffi::export(async_runtime = "tokio")]
pub async fn rpc(base: String, token: String, method: String, body: String) -> Result<String, CoreError> {
    // Same rejection contract as the wasm `rpc`: an api `{ok:false}` carries the
    // RAW envelope text (so the client extracts error_code/description); every
    // other failure flattens to a human message. Flattening the Api case too
    // (the old behavior) silently threw away error_code across the FFI.
    methods::call(&base, &token, &method, &body).await.map_err(|e| match e {
        net::RpcError::Api(envelope) => CoreError::Db(envelope),
        other => CoreError::Db(other.to_string()),
    })
}

// ───────────────────────── native: UniFFI (synchronous facade) ─────────────────────────
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::Arc;
    use crate::store::Store;
    use crate::storage::SqliteStore;
    use crate::{CoreAccount, CoreChannel, CoreConvApp, CoreConversation, CoreError, CoreMessage};

    /// The local store. The UniFFI surface is identical to before (sync methods),
    /// so the Swift/Kotlin host is unchanged; internally it drives the async core
    /// with `block_on` — native storage futures are always ready, so this never
    /// actually blocks on I/O.
    #[derive(uniffi::Object)]
    pub struct MafoldCore {
        inner: Store<SqliteStore>,
    }

    #[uniffi::export]
    impl MafoldCore {
        #[uniffi::constructor]
        pub fn open(db_path: String) -> Result<Arc<Self>, CoreError> {
            let store = SqliteStore::open(&db_path)?;
            Ok(Arc::new(Self { inner: Store::new(store) }))
        }
        pub fn upsert_account(&self, a: CoreAccount) -> Result<(), CoreError> {
            pollster::block_on(self.inner.upsert_account(&a));
            Ok(())
        }
        pub fn upsert_conversation(&self, c: CoreConversation) -> Result<(), CoreError> {
            pollster::block_on(self.inner.upsert_conversation(&c));
            Ok(())
        }
        pub fn upsert_message(&self, m: CoreMessage) -> Result<(), CoreError> {
            pollster::block_on(self.inner.upsert_message(&m));
            Ok(())
        }
        pub fn replace_conversations(&self, convs: Vec<CoreConversation>) -> Result<(), CoreError> {
            pollster::block_on(self.inner.replace_conversations(&convs));
            Ok(())
        }
        pub fn replace_messages(&self, conversation_id: String, msgs: Vec<CoreMessage>) -> Result<(), CoreError> {
            pollster::block_on(self.inner.replace_messages(&conversation_id, &msgs));
            Ok(())
        }
        pub fn delete_messages(&self, conversation_id: String, ids: Vec<String>) -> Result<(), CoreError> {
            pollster::block_on(self.inner.delete_messages(&conversation_id, &ids));
            Ok(())
        }
        pub fn conversations(&self) -> Result<Vec<CoreConversation>, CoreError> {
            Ok(pollster::block_on(self.inner.conversations()))
        }
        pub fn messages(&self, conversation_id: String) -> Result<Vec<CoreMessage>, CoreError> {
            Ok(pollster::block_on(self.inner.messages(&conversation_id)))
        }
        /// A thread view: the root message followed by its replies, oldest→newest.
        /// Reads the single store, so the root copy is always the live one — a
        /// streaming root's `{% generating %}` card is replaced the instant the
        /// channel copy updates, with no per-client reconcile.
        pub fn thread(&self, conversation_id: String, root_id: String) -> Result<Vec<CoreMessage>, CoreError> {
            Ok(pollster::block_on(self.inner.thread(&conversation_id, &root_id)))
        }

        // ── i18n (cloud language packs) — synchronous lookups (no I/O) ──
        /// Look up `key` in the active language (→ English base → key), with
        /// `{name}` interpolation from an optional JSON-object args string.
        pub fn t(&self, key: String, args_json: Option<String>) -> String {
            self.inner.t(&key, args_json.as_deref())
        }
        /// CLDR-plural lookup: the active language's plural form for `n`.
        pub fn plural(&self, key: String, n: f64, args_json: Option<String>) -> String {
            self.inner.plural(&key, n, args_json.as_deref())
        }
        /// `{lang, version, rtl, strings}` for the RN runtime to inject.
        pub fn active_langpack_json(&self) -> String {
            self.inner.active_langpack_json()
        }
        /// The active language code ("zh-Hans" / "en").
        pub fn active_language(&self) -> String {
            self.inner.active_language()
        }
        /// Whether the cloud pack is delivered and `t()` resolves real strings.
        /// Gate the first paint on this — there is no bundled baseline, so a
        /// frame painted before it is a frame of raw `a.b.c` keys.
        pub fn langpack_loaded(&self) -> bool {
            self.inner.langpack_loaded()
        }
        /// Hydrate the active pack from the on-device cache (offline-first; no
        /// network — storage futures are always-ready, so the block_on is free).
        pub fn load_cached_language(&self) {
            pollster::block_on(self.inner.load_cached_language());
        }

        // ── feature flags (flags.rs; .docs/feature-flags.md) ──
        // JSON in/out (same style as the rest of the surface). Mutators return
        // the freshly-resolved `{key: bool}` snapshot — the host updates its
        // observable state from the return value (no cross-FFI subscription).

        /// Mark this process as a dev/internal build (unset flags → dev_default).
        pub fn set_flags_dev(&self, dev: bool) {
            self.inner.set_flags_dev(dev);
        }
        /// Apply a server FlagState JSON (bootstrap / WS push) → resolved snapshot.
        pub fn flags_ingest(&self, state_json: String) -> String {
            pollster::block_on(self.inner.flags_ingest(&state_json))
        }
        /// Set (`Some`) / clear (`None`) a local override → resolved snapshot.
        pub fn flags_set_override(&self, key: String, value: Option<bool>) -> String {
            pollster::block_on(self.inner.flags_set_override(&key, value))
        }
        /// The resolved `{key: bool}` snapshot.
        pub fn flags_resolved(&self) -> String {
            pollster::block_on(self.inner.flags_resolved())
        }
        /// One flag, resolved.
        pub fn flags_enabled(&self, key: String) -> bool {
            pollster::block_on(self.inner.flags_enabled(&key))
        }
        /// The local overrides map (for the dev-toggle UI's tri-state display).
        pub fn flags_overrides(&self) -> String {
            pollster::block_on(self.inner.flags_overrides())
        }
        /// The compile-time registry (key/defaults/metadata) as JSON.
        pub fn flags_registry(&self) -> String {
            crate::flags::registry_json()
        }

        // ── per-device UI state ──
        /// Remember the forum channel last open in a conversation (None = #all),
        /// so reopening the forum lands where the user left off.
        pub fn set_last_channel(&self, conversation_id: String, channel_id: Option<String>) {
            pollster::block_on(self.inner.set_last_channel(&conversation_id, channel_id.as_deref()));
        }
        /// The channel last open in `conversation_id` (None = #all / unset).
        pub fn last_channel(&self, conversation_id: String) -> Option<String> {
            pollster::block_on(self.inner.last_channel(&conversation_id))
        }

        /// Cache a conversation's forum channels (whole-list reconcile).
        pub fn save_channels(&self, conversation_id: String, channels: Vec<CoreChannel>) {
            pollster::block_on(self.inner.replace_channels(&conversation_id, &channels));
        }
        /// Cached channels, pinned first then by `order`. Empty = nothing cached.
        pub fn load_channels(&self, conversation_id: String) -> Vec<CoreChannel> {
            pollster::block_on(self.inner.channels(&conversation_id))
        }
        /// Cache a conversation's installed mini-apps (whole-list reconcile).
        pub fn save_conv_apps(&self, conversation_id: String, apps: Vec<CoreConvApp>) {
            pollster::block_on(self.inner.replace_conv_apps(&conversation_id, &apps));
        }
        /// Cached mini-apps in the server's listed order. Empty = nothing cached.
        pub fn load_conv_apps(&self, conversation_id: String) -> Vec<CoreConvApp> {
            pollster::block_on(self.inner.conv_apps(&conversation_id))
        }
    }

    // set_language does NETWORK I/O (reqwest), so it's an async UniFFI method
    // (Swift `await`) driven by the tokio runtime — block_on can't run reqwest.
    #[uniffi::export(async_runtime = "tokio")]
    impl MafoldCore {
        pub async fn set_language(
            &self,
            base: String,
            token: String,
            code: String,
        ) -> Result<(), CoreError> {
            self.inner
                .set_language(&base, &token, &code)
                .await
                .map_err(CoreError::Db)
        }
    }

    // ── JSON twins of the record-typed surface (mirror of the wasm CoreHandle) ──
    // One JSON contract across both compilation targets: a JS host over UniFFI
    // (mafold-rn) speaks the SAME serde shapes the web wrapper does, so no host
    // hand-writes record converters that could drift. Semantics match the wasm
    // methods verbatim, including how parse failures are swallowed.
    #[uniffi::export]
    impl MafoldCore {
        /// Upsert + reconcile, then RETURN the message's reconciled TIMELINE
        /// window (JSON array, time-ordered; keyed by channel_id falling back
        /// to conversation_id). "null" on a parse failure — callers fall back.
        pub fn upsert_message_json(&self, json: String) -> String {
            if let Ok(m) = serde_json::from_str::<CoreMessage>(&json) {
                let timeline = m.channel_id.clone().unwrap_or_else(|| m.conversation_id.clone());
                pollster::block_on(self.inner.upsert_message(&m));
                let win = pollster::block_on(self.inner.messages(&timeline));
                return serde_json::to_string(&win).unwrap_or_else(|_| "null".into());
            }
            "null".into()
        }
        pub fn upsert_conversation_json(&self, json: String) {
            if let Ok(c) = serde_json::from_str::<CoreConversation>(&json) {
                pollster::block_on(self.inner.upsert_conversation(&c));
            }
        }
        pub fn upsert_account_json(&self, json: String) {
            if let Ok(a) = serde_json::from_str::<CoreAccount>(&json) {
                pollster::block_on(self.inner.upsert_account(&a));
            }
        }
        pub fn replace_conversations_json(&self, json: String) {
            if let Ok(cs) = serde_json::from_str::<Vec<CoreConversation>>(&json) {
                pollster::block_on(self.inner.replace_conversations(&cs));
            }
        }
        pub fn replace_messages_json(&self, conversation_id: String, json: String) {
            if let Ok(ms) = serde_json::from_str::<Vec<CoreMessage>>(&json) {
                pollster::block_on(self.inner.replace_messages(&conversation_id, &ms));
            }
        }
        pub fn delete_messages_json(&self, conversation_id: String, ids_json: String) {
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&ids_json) {
                pollster::block_on(self.inner.delete_messages(&conversation_id, &ids));
            }
        }
        pub fn messages_json(&self, conversation_id: String) -> String {
            serde_json::to_string(&pollster::block_on(self.inner.messages(&conversation_id)))
                .unwrap_or_default()
        }
        /// Thread view (root + replies, oldest→newest) as JSON — the root copy
        /// is the live one (see `thread`).
        pub fn thread_json(&self, conversation_id: String, root_id: String) -> String {
            serde_json::to_string(&pollster::block_on(self.inner.thread(&conversation_id, &root_id)))
                .unwrap_or_default()
        }
        pub fn conversations_json(&self) -> String {
            serde_json::to_string(&pollster::block_on(self.inner.conversations()))
                .unwrap_or_default()
        }
        pub fn save_channels_json(&self, conversation_id: String, channels_json: String) {
            let list: Vec<CoreChannel> = serde_json::from_str(&channels_json).unwrap_or_default();
            pollster::block_on(self.inner.replace_channels(&conversation_id, &list));
        }
        pub fn load_channels_json(&self, conversation_id: String) -> String {
            serde_json::to_string(&pollster::block_on(self.inner.channels(&conversation_id)))
                .unwrap_or_else(|_| "[]".into())
        }
        pub fn save_conv_apps_json(&self, conversation_id: String, apps_json: String) {
            let list: Vec<CoreConvApp> = serde_json::from_str(&apps_json).unwrap_or_default();
            pollster::block_on(self.inner.replace_conv_apps(&conversation_id, &list));
        }
        pub fn load_conv_apps_json(&self, conversation_id: String) -> String {
            serde_json::to_string(&pollster::block_on(self.inner.conv_apps(&conversation_id)))
                .unwrap_or_else(|_| "[]".into())
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use native::MafoldCore;

// ───────────────────────── web: wasm-bindgen (async → Promises) ─────────────────────────
#[cfg(target_arch = "wasm32")]
mod web {
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::future_to_promise;
    use crate::store::Store;
    use crate::storage::IdbStore;
    use crate::{CoreAccount, CoreConversation, CoreMessage};

    #[wasm_bindgen(js_name = coreVersion)]
    pub fn core_version() -> String { crate::core_version() }

    /// The connection-provider registry, display-ready JSON.
    ///
    /// Serving it from the core is what keeps there being ONE registry: the cli
    /// reads `mafold_types::PROVIDERS` natively and the web reads the same const
    /// through here, so adding a provider is one row in `connections.rs` and no
    /// client edit at all. A hand-kept `{id → name, icon}` map in the web would
    /// be a second registry that silently drifts — and the client that drifts is
    /// the one the user is looking at.
    #[wasm_bindgen(js_name = connectionProviders)]
    pub fn connection_providers() -> String {
        serde_json::to_string(&mafold_types::connections::provider_infos())
            .unwrap_or_else(|_| "[]".into())
    }

    /// Sync engine, stage 1: the core makes the API call itself (POST
    /// {base}/{method}, Bearer token, {ok,result} envelope) → returns the result
    /// JSON. Validates the method name against the api route table (`methods`)
    /// before any I/O. Rejection value contract (web's `rpc()` in lib/api.ts
    /// parses it): an api `{ok:false}` rejects with the RAW ENVELOPE TEXT (so
    /// the client extracts error_code/description); anything else rejects with
    /// a plain human message.
    #[wasm_bindgen(js_name = rpc)]
    pub fn rpc(base: String, token: String, method: String, body: String) -> js_sys::Promise {
        future_to_promise(async move {
            match crate::methods::call(&base, &token, &method, &body).await {
                Ok(result) => Ok(JsValue::from_str(&result)),
                Err(crate::net::RpcError::Api(envelope)) => Err(JsValue::from_str(&envelope)),
                Err(e) => Err(JsValue::from_str(&e.to_string())),
            }
        })
    }

    /// Sync engine stage 2: the core opens the realtime WS itself (reconnecting
    /// on drop) and streams each event (text frame) to `onEvent`. Returns a
    /// handle whose `close()` stops the loop (call it on logout / account switch).
    #[wasm_bindgen(js_name = connectWs)]
    pub fn connect_ws(url: String, on_event: js_sys::Function) -> WsHandle {
        let closed = std::rc::Rc::new(std::cell::Cell::new(false));
        crate::ws::connect(&url, on_event, closed.clone());
        WsHandle { closed }
    }

    // ── connections vault ──
    // The browser is a REAL vault device here, not a viewer: it owns an X25519
    // keypair and can seal new credentials. What it must never do is let the
    // master key become a JS value — the agreed model is "device key on disk,
    // UMK in memory only", and a base64 UMK handed to JS would sit in strings
    // no one can zeroize, reachable from any script on the page. So the key
    // stays in wasm linear memory behind this handle and JS only ever passes
    // payloads through it.

    /// Mint a device keypair. The secret is returned ONCE, for the caller to
    /// persist (browser: IndexedDB); it is never sent anywhere.
    #[wasm_bindgen(js_name = vaultGenerateDevice)]
    pub fn vault_generate_device() -> String {
        let d = crate::vault::generate_device();
        serde_json::json!({
            "secret": d.secret,
            "public": d.public,
            "fingerprint": crate::vault::fingerprint(&d.public),
        })
        .to_string()
    }

    /// The public half + fingerprint for an already-stored secret.
    #[wasm_bindgen(js_name = vaultDeviceIdentity)]
    pub fn vault_device_identity(secret: String) -> Result<String, JsValue> {
        let public = crate::vault::public_from_secret(&secret)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(serde_json::json!({
            "public": public,
            "fingerprint": crate::vault::fingerprint(&public),
        })
        .to_string())
    }

    #[wasm_bindgen(js_name = vaultFingerprint)]
    pub fn vault_fingerprint(public_key: String) -> String {
        crate::vault::fingerprint(&public_key)
    }

    /// An unlocked vault. Dropping it (`free()`) drops the master key.
    #[wasm_bindgen]
    pub struct Vault {
        umk: crate::vault::Key,
        key_id: String,
    }

    /// Open the wrap an approving device left for this one. Fails when the wrap
    /// was addressed to a different key — which is exactly what a substituted
    /// public key would produce, so this failing is the design working.
    #[wasm_bindgen(js_name = vaultUnlock)]
    pub fn vault_unlock(device_secret: String, wrapped_umk: String, key_id: String) -> Result<Vault, JsValue> {
        let umk = crate::vault::unwrap_umk(&device_secret, &wrapped_umk)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(Vault { umk, key_id })
    }

    /// Mint a brand-new vault — only valid when the account has no enrolled
    /// device yet (the server reports `first`). The caller must immediately
    /// wrap it for itself, or the credentials it stores become unopenable.
    #[wasm_bindgen(js_name = vaultCreate)]
    pub fn vault_create() -> Vault {
        Vault { umk: crate::vault::Key::random(), key_id: crate::vault::new_key_id() }
    }

    #[wasm_bindgen]
    impl Vault {
        #[wasm_bindgen(getter, js_name = keyId)]
        pub fn key_id(&self) -> String {
            self.key_id.clone()
        }

        /// Wrap this master key for a device's public key — the approval step.
        #[wasm_bindgen(js_name = wrapFor)]
        pub fn wrap_for(&self, recipient_public: String) -> Result<String, JsValue> {
            crate::vault::wrap_umk_for(&recipient_public, &self.umk)
                .map_err(|e| JsValue::from_str(&e.to_string()))
        }

        /// Seal a connection payload → `{blob, wrapped_dek}`.
        #[wasm_bindgen(js_name = sealPayload)]
        pub fn seal_payload(&self, payload_json: String) -> String {
            let s = crate::vault::seal_payload(&self.umk, &payload_json);
            serde_json::json!({ "blob": s.blob, "wrapped_dek": s.wrapped_dek }).to_string()
        }

        /// Open one → the payload JSON.
        #[wasm_bindgen(js_name = openPayload)]
        pub fn open_payload(&self, blob: String, wrapped_dek: String) -> Result<String, JsValue> {
            crate::vault::open_payload(&self.umk, &blob, &wrapped_dek)
                .map_err(|e| JsValue::from_str(&e.to_string()))
        }

        /// Hand one socket event to the core. Resolves `true` when the core
        /// took it, so the host can fall through to its own dispatch otherwise.
        ///
        /// This is the browser's half of "a connection is answered by the core,
        /// like a message is received". The host does not decide anything: it
        /// forwards the frame, and the same Rust that answers on a laptop
        /// answers here. Nothing about which events matter, how a call is
        /// claimed, or how a result is shaped is expressible in the host.
        #[wasm_bindgen(js_name = handleEvent)]
        pub async fn handle_event(&self, base: String, token: String, envelope: String) -> bool {
            let mut rt = crate::connections::Runtime::new(&base, &token, self.umk.clone());
            crate::connections::handle_event(&mut rt, &envelope).await
        }
    }

    /// Returned by `connectWs`; `close()` stops the reconnect loop.
    #[wasm_bindgen]
    pub struct WsHandle {
        closed: std::rc::Rc<std::cell::Cell<bool>>,
    }
    #[wasm_bindgen]
    impl WsHandle {
        pub fn close(&self) {
            self.closed.set(true);
        }
    }

    /// Web handle over the async core, persisted to IndexedDB. Methods return JS
    /// Promises; the handle holds an `Rc` so each future owns its state (no
    /// &self-across-await borrow). Same `Store` logic as native — only the
    /// Storage backend differs (IndexedDB here, SQLite on native).
    #[wasm_bindgen]
    pub struct CoreHandle {
        inner: Rc<Store<IdbStore>>,
    }

    #[wasm_bindgen]
    impl CoreHandle {
        /// Open (or create) the IndexedDB-backed core for this account.
        pub fn open(name: String) -> js_sys::Promise {
            future_to_promise(async move {
                match IdbStore::open(&name).await {
                    Ok(store) => Ok(JsValue::from(CoreHandle { inner: Rc::new(Store::new(store)) })),
                    Err(e) => Err(JsValue::from_str(&e)),
                }
            })
        }
        /// Upsert + reconcile, then RETURN the message's reconciled TIMELINE
        /// window (JSON array, time-ordered) — so the host view can be a
        /// projection of the core's reconcile (membership + order decided HERE,
        /// once) instead of re-implementing the client_msg_id rule in JS.
        /// Returns "null" on a parse failure (callers fall back gracefully).
        #[wasm_bindgen(js_name = upsertMessage)]
        pub fn upsert_message(&self, json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(m) = serde_json::from_str::<CoreMessage>(&json) {
                    let timeline = m.channel_id.clone().unwrap_or_else(|| m.conversation_id.clone());
                    inner.upsert_message(&m).await;
                    let win = inner.messages(&timeline).await;
                    return Ok(JsValue::from_str(
                        &serde_json::to_string(&win).unwrap_or_else(|_| "null".into()),
                    ));
                }
                Ok(JsValue::from_str("null"))
            })
        }
        #[wasm_bindgen(js_name = upsertConversation)]
        pub fn upsert_conversation(&self, json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(c) = serde_json::from_str::<CoreConversation>(&json) {
                    inner.upsert_conversation(&c).await;
                }
                Ok(JsValue::UNDEFINED)
            })
        }
        #[wasm_bindgen(js_name = upsertAccount)]
        pub fn upsert_account(&self, json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(a) = serde_json::from_str::<CoreAccount>(&json) { inner.upsert_account(&a).await; }
                Ok(JsValue::UNDEFINED)
            })
        }
        #[wasm_bindgen(js_name = replaceConversations)]
        pub fn replace_conversations(&self, json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(cs) = serde_json::from_str::<Vec<CoreConversation>>(&json) { inner.replace_conversations(&cs).await; }
                Ok(JsValue::UNDEFINED)
            })
        }
        #[wasm_bindgen(js_name = replaceMessages)]
        pub fn replace_messages(&self, conv: String, json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(ms) = serde_json::from_str::<Vec<CoreMessage>>(&json) { inner.replace_messages(&conv, &ms).await; }
                Ok(JsValue::UNDEFINED)
            })
        }
        #[wasm_bindgen(js_name = deleteMessages)]
        pub fn delete_messages(&self, conv: String, ids_json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                if let Ok(ids) = serde_json::from_str::<Vec<String>>(&ids_json) { inner.delete_messages(&conv, &ids).await; }
                Ok(JsValue::UNDEFINED)
            })
        }
        pub fn messages(&self, conv: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&serde_json::to_string(&inner.messages(&conv).await).unwrap_or_default()))
            })
        }
        /// A thread view (root + its replies, oldest→newest) as JSON — the root
        /// copy read here is the live one, so a streaming root never sticks on a
        /// stale snapshot. See the native `thread` for the rationale.
        pub fn thread(&self, conv: String, root: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&serde_json::to_string(&inner.thread(&conv, &root).await).unwrap_or_default()))
            })
        }
        pub fn conversations(&self) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&serde_json::to_string(&inner.conversations().await).unwrap_or_default()))
            })
        }

        // ── i18n (cloud language packs) ──
        /// Synchronous string lookup (active → English base → key) + `{name}`
        /// interpolation. No I/O, so it returns a String directly (not a Promise).
        pub fn t(&self, key: String, args_json: Option<String>) -> String {
            self.inner.t(&key, args_json.as_deref())
        }
        /// Synchronous CLDR-plural lookup for `n`.
        pub fn plural(&self, key: String, n: f64, args_json: Option<String>) -> String {
            self.inner.plural(&key, n, args_json.as_deref())
        }
        #[wasm_bindgen(js_name = activeLangpackJson)]
        pub fn active_langpack_json(&self) -> String {
            self.inner.active_langpack_json()
        }
        #[wasm_bindgen(js_name = activeLanguage)]
        pub fn active_language(&self) -> String {
            self.inner.active_language()
        }
        /// Whether the cloud pack is delivered and `t()` resolves real strings.
        /// Gate the first paint on this — there is no bundled baseline, so a
        /// frame painted before it is a frame of raw `a.b.c` keys.
        #[wasm_bindgen(js_name = langpackLoaded)]
        pub fn langpack_loaded(&self) -> bool {
            self.inner.langpack_loaded()
        }
        /// Switch language: delta-sync the English base + chosen language, persist,
        /// swap the active pack. Returns a Promise (network I/O).
        #[wasm_bindgen(js_name = setLanguage)]
        pub fn set_language(&self, base: String, token: String, code: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                match inner.set_language(&base, &token, &code).await {
                    Ok(()) => Ok(JsValue::UNDEFINED),
                    Err(e) => Err(JsValue::from_str(&e)),
                }
            })
        }
        /// Hydrate the active pack from IndexedDB (offline-first), before
        /// `setLanguage` refreshes from the network.
        #[wasm_bindgen(js_name = loadCachedLanguage)]
        pub fn load_cached_language(&self) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                inner.load_cached_language().await;
                Ok(JsValue::UNDEFINED)
            })
        }

        // ── feature flags (flags.rs; .docs/feature-flags.md) ──
        // Mutators resolve to the fresh `{key: bool}` snapshot JSON — the JS
        // wrapper feeds it straight into the zustand store (its reactivity is
        // the change-notifier; no cross-wasm subscription needed).

        /// Mark this session as a dev build (unset flags → dev_default).
        #[wasm_bindgen(js_name = setFlagsDev)]
        pub fn set_flags_dev(&self, dev: bool) {
            self.inner.set_flags_dev(dev);
        }
        /// Apply a server FlagState JSON → Promise<resolved snapshot JSON>.
        #[wasm_bindgen(js_name = flagsIngest)]
        pub fn flags_ingest(&self, state_json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&inner.flags_ingest(&state_json).await))
            })
        }
        /// Set (`on` bool) / clear (`null`) a local override → Promise<snapshot>.
        #[wasm_bindgen(js_name = flagsSetOverride)]
        pub fn flags_set_override(&self, key: String, value: Option<bool>) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&inner.flags_set_override(&key, value).await))
            })
        }
        /// Promise<resolved `{key: bool}` snapshot JSON>.
        #[wasm_bindgen(js_name = flagsResolved)]
        pub fn flags_resolved(&self) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&inner.flags_resolved().await))
            })
        }
        /// Promise<the local overrides map JSON> (dev-toggle tri-state display).
        #[wasm_bindgen(js_name = flagsOverrides)]
        pub fn flags_overrides(&self) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(JsValue::from_str(&inner.flags_overrides().await))
            })
        }
        /// The compile-time registry (key/defaults/metadata) as JSON. Static.
        #[wasm_bindgen(js_name = flagsRegistry)]
        pub fn flags_registry(&self) -> String {
            crate::flags::registry_json()
        }

        // ── per-device UI state ──
        /// Remember (string) / forget (undefined = #all) the forum channel last
        /// open in a conversation — read back by `lastChannel` on reopen.
        #[wasm_bindgen(js_name = setLastChannel)]
        pub fn set_last_channel(&self, conv: String, channel: Option<String>) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                inner.set_last_channel(&conv, channel.as_deref()).await;
                Ok(JsValue::UNDEFINED)
            })
        }
        /// Promise<string | undefined> — the channel last open in `conv`.
        #[wasm_bindgen(js_name = lastChannel)]
        pub fn last_channel(&self, conv: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                Ok(match inner.last_channel(&conv).await {
                    Some(c) => JsValue::from_str(&c),
                    None => JsValue::UNDEFINED,
                })
            })
        }

        // ── local-first caches: paint on open, refresh behind ──
        /// Cache a conversation's forum channels (whole-list reconcile).
        #[wasm_bindgen(js_name = saveChannels)]
        pub fn save_channels(&self, conv: String, channels_json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                let list: Vec<crate::CoreChannel> =
                    serde_json::from_str(&channels_json).unwrap_or_default();
                inner.replace_channels(&conv, &list).await;
                Ok(JsValue::UNDEFINED)
            })
        }
        /// Promise<string> — cached channels as JSON, pinned first then `order`.
        #[wasm_bindgen(js_name = loadChannels)]
        pub fn load_channels(&self, conv: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                let list = inner.channels(&conv).await;
                Ok(JsValue::from_str(&serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())))
            })
        }
        /// Cache a conversation's installed mini-apps (whole-list reconcile).
        #[wasm_bindgen(js_name = saveConvApps)]
        pub fn save_conv_apps(&self, conv: String, apps_json: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                let list: Vec<crate::CoreConvApp> =
                    serde_json::from_str(&apps_json).unwrap_or_default();
                inner.replace_conv_apps(&conv, &list).await;
                Ok(JsValue::UNDEFINED)
            })
        }
        /// Promise<string> — cached mini-apps as JSON, in the server's order.
        #[wasm_bindgen(js_name = loadConvApps)]
        pub fn load_conv_apps(&self, conv: String) -> js_sys::Promise {
            let inner = self.inner.clone();
            future_to_promise(async move {
                let list = inner.conv_apps(&conv).await;
                Ok(JsValue::from_str(&serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())))
            })
        }
    }
}

// ───────────────────────── tests (parity net — UNCHANGED behavior) ─────────────
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn acct(u: &str) -> CoreAccount {
        CoreAccount {
            username: u.into(), display_name: u.to_uppercase(), kind: "human".into(),
            avatar: None, parent_username: None, template: None, language: None,
            verified: false,
        }
    }

    #[test]
    fn store_round_trip() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        core.upsert_conversation(CoreConversation {
            id: "c1".into(), kind: "direct".into(), title: None,
            participants: vec![acct("alice"), acct("bob")],
            updated_at_ms: 100, unread_count: 2, last_message: None, is_forum: false, forum_member_channels: false,
            member_add_members: false, member_edit_info: false, member_add_bots: false,
        }).unwrap();
        core.upsert_message(CoreMessage {
            id: "m1".into(), conversation_id: "c1".into(), sender: acct("alice"),
            content: "hello".into(), created_at_ms: 50, finalized_at_ms: Some(50), client_msg_id: None, thread_root_id: None, channel_id: None, payload: None,
        }).unwrap();
        core.upsert_message(CoreMessage {
            id: "m2".into(), conversation_id: "c1".into(), sender: acct("bob"),
            content: "world".into(), created_at_ms: 150, finalized_at_ms: Some(150), client_msg_id: None, thread_root_id: None, channel_id: None, payload: None,
        }).unwrap();

        let msgs = core.messages("c1".into()).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");        // oldest first
        assert_eq!(msgs[0].sender.display_name, "ALICE");

        let convs = core.conversations().unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].kind, "direct");
        assert_eq!(convs[0].participants.len(), 2);
        assert_eq!(convs[0].unread_count, 2);
        // last_message is the newest → never "No messages yet" while msgs exist
        assert_eq!(convs[0].last_message.as_ref().unwrap().content, "world");
        assert!(!core_version().is_empty());
    }

    #[test]
    fn replace_prunes_gone_conversations() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        let c = |id: &str, ts: i64| CoreConversation {
            id: id.into(), kind: "direct".into(), title: None,
            participants: vec![acct("me"), acct(id)], updated_at_ms: ts, unread_count: 0, last_message: None, is_forum: false, forum_member_channels: false,
            member_add_members: false, member_edit_info: false, member_add_bots: false,
        };
        core.replace_conversations(vec![c("a", 1), c("b", 2)]).unwrap();
        assert_eq!(core.conversations().unwrap().len(), 2);
        // 'a' dropped server-side → gone locally after reconcile; newest first.
        core.replace_conversations(vec![c("b", 3)]).unwrap();
        let convs = core.conversations().unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "b");
    }

    #[test]
    fn json_twins_mirror_wasm_semantics() {
        // The UniFFI *_json surface (mafold-rn) must behave exactly like the
        // wasm CoreHandle: same serde shapes, upsert returns the reconciled
        // timeline window keyed by channel_id ?? conversation_id.
        let core = MafoldCore::open(":memory:".into()).unwrap();
        core.upsert_conversation_json(
            r#"{"id":"c1","kind":"direct","title":null,"participants":[],
                "updated_at_ms":100,"unread_count":0,"last_message":null,
                "is_forum":false,"forum_member_channels":false}"#.into(),
        );
        let win = core.upsert_message_json(
            r#"{"id":"m1","conversation_id":"c1",
                "sender":{"username":"ops","display_name":"Ops","kind":"human",
                          "avatar_url":null,"parent_username":null,"template":null,
                          "language":null,"verified":false},
                "content":"hi","created_at_ms":50,"finalized_at_ms":50,
                "client_msg_id":null,"thread_root_id":null,"channel_id":null,
                "payload":null}"#.into(),
        );
        let msgs: Vec<CoreMessage> = serde_json::from_str(&win).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi");
        // channel message lands in the CHANNEL timeline, not the conv one
        let win2 = core.upsert_message_json(
            r#"{"id":"m2","conversation_id":"c1",
                "sender":{"username":"ops","display_name":"Ops","kind":"human",
                          "avatar_url":null,"parent_username":null,"template":null,
                          "language":null,"verified":false},
                "content":"in-channel","created_at_ms":60,"finalized_at_ms":60,
                "client_msg_id":null,"thread_root_id":null,"channel_id":"ch9",
                "payload":null}"#.into(),
        );
        let ch_msgs: Vec<CoreMessage> = serde_json::from_str(&win2).unwrap();
        assert_eq!(ch_msgs.len(), 1);
        assert_eq!(ch_msgs[0].id, "m2");
        assert_eq!(serde_json::from_str::<Vec<CoreMessage>>(&core.messages_json("c1".into())).unwrap().len(), 1);
        // parse failure is swallowed, mirrors wasm
        assert_eq!(core.upsert_message_json("not json".into()), "null");
        assert_eq!(serde_json::from_str::<Vec<CoreConversation>>(&core.conversations_json()).unwrap().len(), 1);
    }

    #[test]
    fn wire_message_converts_to_core() {
        // A server wire `Message` (the exact JSON the API serves) → CoreMessage.
        // Proves the core adopts the shared mafold-types model end to end.
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "conversation_id": "00000000-0000-0000-0000-0000000000c1",
            "sender": { "username": "ops", "display_name": "Ops", "kind": "human" },
            "content": "hello",
            "created_at": "2026-06-23T00:00:00Z",
            "finalized_at": "2026-06-23T00:00:01Z",
            "reactions": [],
            "client_msg_id": "cmid-9"
        }"#;
        let wm: mafold_types::Message = serde_json::from_str(json).unwrap();
        let cm: CoreMessage = (&wm).into();
        assert_eq!(cm.id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(cm.conversation_id, "00000000-0000-0000-0000-0000000000c1");
        assert_eq!(cm.sender.username, "ops");
        assert_eq!(cm.sender.kind, "human");
        assert_eq!(cm.content, "hello");
        assert_eq!(cm.created_at_ms, 1782172800000);          // 2026-06-23T00:00:00Z
        assert_eq!(cm.finalized_at_ms, Some(1782172801000));  // +1s
        assert_eq!(cm.client_msg_id.as_deref(), Some("cmid-9"));
        assert!(cm.payload.as_deref().unwrap().contains("\"content\":\"hello\""));
    }

    #[test]
    fn delete_messages_removes_by_id() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        for (id, ts) in [("a", 10i64), ("b", 20), ("c", 30)] {
            core.upsert_message(CoreMessage {
                id: id.into(), conversation_id: "k".into(), sender: acct("x"),
                content: id.into(), created_at_ms: ts, finalized_at_ms: Some(ts),
                client_msg_id: None, thread_root_id: None, channel_id: None, payload: None,
            }).unwrap();
        }
        core.delete_messages("k".into(), vec!["b".into(), "c".into()]).unwrap();
        let msgs = core.messages("k".into()).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "a");
    }

    #[test]
    fn optimistic_echo_reconciles_by_client_msg_id() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        // Optimistic send: a local placeholder (temp id, local ts) carrying the
        // client_msg_id, with an optimistic payload.
        core.upsert_message(CoreMessage {
            id: "temp-1".into(), conversation_id: "c".into(), sender: acct("me"),
            content: "hi".into(), created_at_ms: 100, finalized_at_ms: None,
            client_msg_id: Some("cmid-1".into()), thread_root_id: None, channel_id: None, payload: Some("{\"optimistic\":1}".into()),
        }).unwrap();
        // Server echo: REAL id + server ts, SAME client_msg_id, payload present.
        core.upsert_message(CoreMessage {
            id: "real-1".into(), conversation_id: "c".into(), sender: acct("me"),
            content: "hi".into(), created_at_ms: 105, finalized_at_ms: Some(105),
            client_msg_id: Some("cmid-1".into()), thread_root_id: None, channel_id: None, payload: Some("{\"server\":1}".into()),
        }).unwrap();

        let msgs = core.messages("c".into()).unwrap();
        assert_eq!(msgs.len(), 1, "echo must REPLACE the optimistic placeholder, not duplicate");
        assert_eq!(msgs[0].id, "real-1");                       // server id wins
        assert_eq!(msgs[0].finalized_at_ms, Some(105));
        assert_eq!(msgs[0].payload.as_deref(), Some("{\"server\":1}"));

        // A payload-less echo (e.g. a later messageComplete) keeps the optimistic
        // payload forward rather than blanking it.
        core.upsert_message(CoreMessage {
            id: "real-2".into(), conversation_id: "c".into(), sender: acct("me"),
            content: "yo".into(), created_at_ms: 200, finalized_at_ms: None,
            client_msg_id: Some("cmid-2".into()), thread_root_id: None, channel_id: None, payload: Some("{\"opt2\":1}".into()),
        }).unwrap();
        core.upsert_message(CoreMessage {
            id: "real-2b".into(), conversation_id: "c".into(), sender: acct("me"),
            content: "yo".into(), created_at_ms: 205, finalized_at_ms: Some(205),
            client_msg_id: Some("cmid-2".into()), thread_root_id: None, channel_id: None, payload: None,
        }).unwrap();
        let msgs = core.messages("c".into()).unwrap();
        assert_eq!(msgs.len(), 2);
        let m2 = msgs.iter().find(|m| m.client_msg_id.as_deref() == Some("cmid-2")).unwrap();
        assert_eq!(m2.id, "real-2b");
        assert_eq!(m2.payload.as_deref(), Some("{\"opt2\":1}"), "payload carried forward on payload-less echo");
    }

    /// End-to-end: drive `set_language` against a LIVE api (delta sync + cache +
    /// the synchronous `t`/`plural`). Skipped unless `MAFOLD_TEST_API` points at a
    /// running server that already has the `en` + `zh-Hans` packs published
    /// (the langpacks/*.json seeds). Run:
    ///   MAFOLD_TEST_API=http://127.0.0.1:4100/api MAFOLD_TEST_TOKEN=dev:ops \
    ///     cargo test set_language_e2e -- --nocapture
    #[tokio::test]
    async fn set_language_e2e_live_api() {
        let Ok(base) = std::env::var("MAFOLD_TEST_API") else { return };
        let token = std::env::var("MAFOLD_TEST_TOKEN").unwrap_or_else(|_| "dev:ops".into());
        let core = MafoldCore::open(":memory:".into()).unwrap();

        // Full pack (from_version 0): switch to zh-Hans, then read translated +
        // interpolated + plural values synchronously.
        core.set_language(base.clone(), token.clone(), "zh-Hans".into()).await.unwrap();
        assert_eq!(core.active_language(), "zh-Hans");
        assert_eq!(core.t("settings.title".into(), None), "设置");
        assert_eq!(core.t("card.quote.buy".into(), Some(r#"{"symbol":"AAPL"}"#.into())), "买入 AAPL");
        assert_eq!(core.plural("profile.members.count".into(), 3.0, None), "3 名成员");
        let j = core.active_langpack_json();
        assert!(j.contains("\"lang\":\"zh-Hans\"") && j.contains("设置"), "effective json: {j}");

        // Second switch to the SAME language exercises the delta path
        // (from_version > 0 → empty delta, values preserved from cache).
        core.set_language(base.clone(), token.clone(), "zh-Hans".into()).await.unwrap();
        assert_eq!(core.t("settings.title".into(), None), "设置");

        // Switch to English: the base values.
        core.set_language(base, token, "en".into()).await.unwrap();
        assert_eq!(core.active_language(), "en");
        assert_eq!(core.t("settings.title".into(), None), "Settings");
        assert_eq!(core.plural("profile.members.count".into(), 1.0, None), "1 member");
        assert_eq!(core.plural("profile.members.count".into(), 5.0, None), "5 members");
        println!("set_language_e2e_live_api: OK");
    }

    /// The channel/app caches are WHOLE-LIST reconciles, not merges: whatever
    /// the server last listed is exactly what's cached. A channel that was
    /// deleted server-side must not survive in the picker, and one conversation's
    /// list must never bleed into another's.
    #[test]
    fn channel_and_app_caches_replace_and_scope() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        let ch = |id: &str, order: i32, pinned: bool, activity: i64| CoreChannel {
            id: id.into(),
            conversation_id: "c1".into(),
            name: id.into(),
            order,
            icon: None,
            unread_count: 0,
            closed: false,
            pinned,
            archived: false,
            created_by: None,
            activity_at_ms: activity,
        };

        core.save_channels("c1".into(), vec![
            ch("stale", 1, false, 100),
            ch("fresh", 2, false, 900),
            ch("p", 9, true, 1),          // pinned outranks recency
        ]);
        core.save_channels("c2".into(), vec![ch("z", 0, false, 0)]);

        // Pinned → most recent → order. Note `fresh` beats `stale` DESPITE a
        // higher `order`: recency outranks it, and order only breaks ties.
        let got: Vec<String> = core.load_channels("c1".into()).iter().map(|c| c.id.clone()).collect();
        assert_eq!(got, ["p", "fresh", "stale"], "pinned, then most-recently-active");
        // Scoped per conversation.
        assert_eq!(core.load_channels("c2".into()).len(), 1);

        // Re-save WITHOUT "a" → it's gone, not merged back in.
        core.save_channels("c1".into(), vec![ch("fresh", 2, false, 900)]);
        let got: Vec<String> = core.load_channels("c1".into()).iter().map(|c| c.id.clone()).collect();
        assert_eq!(got, ["fresh"], "dropped channels must not survive the reconcile");
        assert_eq!(core.load_channels("c2".into()).len(), 1, "other conversations untouched");

        let app = |id: &str, order: i32| CoreConvApp {
            app_id: id.into(),
            order,
            entry_json: format!(r#"{{"id":"{id}"}}"#),
        };
        core.save_conv_apps("c1".into(), vec![app("o/two", 1), app("o/one", 0)]);
        let got: Vec<String> = core.load_conv_apps("c1".into()).iter().map(|a| a.app_id.clone()).collect();
        assert_eq!(got, ["o/one", "o/two"], "server order preserved");
        core.save_conv_apps("c1".into(), vec![]);
        assert!(core.load_conv_apps("c1".into()).is_empty(), "uninstalling clears the cache");
    }

    #[test]
    fn payload_coalesces_on_reupsert() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        let mut m = CoreMessage {
            id: "m".into(), conversation_id: "c".into(), sender: acct("a"),
            content: "hi".into(), created_at_ms: 10, finalized_at_ms: None,
            client_msg_id: None, thread_root_id: None, channel_id: None, payload: Some("{\"full\":1}".into()),
        };
        core.upsert_message(m.clone()).unwrap();
        // re-upsert with payload None (e.g. a finalize) must KEEP the payload.
        m.payload = None;
        m.finalized_at_ms = Some(20);
        core.upsert_message(m).unwrap();
        let got = core.messages("c".into()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload.as_deref(), Some("{\"full\":1}"));
        assert_eq!(got[0].finalized_at_ms, Some(20));
    }

    #[test]
    fn thread_view_splits_channel_and_reflects_live_root() {
        let core = MafoldCore::open(":memory:".into()).unwrap();
        let msg = |id: &str, ts: i64, content: &str, root: Option<&str>| CoreMessage {
            id: id.into(), conversation_id: "c".into(), sender: acct("bot"),
            content: content.into(), created_at_ms: ts, finalized_at_ms: None,
            client_msg_id: None, thread_root_id: root.map(|s| s.into()), channel_id: None, payload: None,
        };
        core.upsert_conversation(CoreConversation {
            id: "c".into(), kind: "group".into(), title: None,
            participants: vec![acct("me"), acct("bot")], updated_at_ms: 0, unread_count: 0, last_message: None, is_forum: false, forum_member_channels: false,
            member_add_members: false, member_edit_info: false, member_add_bots: false,
        }).unwrap();

        // A top-level root that is STILL generating, a plain channel message, and
        // two replies under the root.
        core.upsert_message(msg("root", 100, "thinking… {% generating %}", None)).unwrap();
        core.upsert_message(msg("chan", 110, "unrelated channel msg", None)).unwrap();
        core.upsert_message(msg("r1", 120, "first reply", Some("root"))).unwrap();
        core.upsert_message(msg("r2", 130, "second reply", Some("root"))).unwrap();

        // messages() is the CHANNEL only — replies are excluded.
        let chan = core.messages("c".into()).unwrap();
        assert_eq!(chan.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["root", "chan"]);

        // thread() = root first, then replies in time order.
        let thr = core.thread("c".into(), "root".into()).unwrap();
        assert_eq!(thr.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["root", "r1", "r2"]);
        assert!(thr[0].content.contains("{% generating %}"));

        // The bug: the streaming root finalizes (same id, no generating card). The
        // thread view must reflect the LIVE root, not a stale snapshot.
        core.upsert_message(CoreMessage {
            finalized_at_ms: Some(140), ..msg("root", 100, "final answer", None)
        }).unwrap();
        let thr = core.thread("c".into(), "root".into()).unwrap();
        assert_eq!(thr[0].content, "final answer");
        assert_eq!(thr[0].finalized_at_ms, Some(140));
        assert_eq!(thr.len(), 3, "replies still present after the root finalize");

        // A reply never becomes the channel's dialog preview.
        let convs = core.conversations().unwrap();
        assert_eq!(convs[0].last_message.as_ref().unwrap().id, "chan");

        // Refreshing the channel window (server history = top-level only) must NOT
        // drop the open thread's replies.
        core.replace_messages("c".into(), vec![
            msg("root", 100, "final answer", None), msg("chan", 110, "unrelated channel msg", None),
        ]).unwrap();
        let thr = core.thread("c".into(), "root".into()).unwrap();
        assert_eq!(thr.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["root", "r1", "r2"],
                   "replies survive a channel-window replace");
    }
}
