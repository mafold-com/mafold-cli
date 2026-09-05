//! Portable ASYNC core logic — written ONCE against `Storage`, compiles native+wasm.
//!
//! "Queries" (messages ordered within a conversation, the dialog list with its
//! last message) are done HERE in Rust over a KV store via sortable composite
//! keys + range scans — no SQL, no DB coupling. Accounts are DENORMALIZED inline
//! into messages (`sender`) and conversations (`participants`), so there are no
//! joins: a message/conversation value carries everything it needs.
//!
//! KV layout:
//!   "acct" : username                     -> CoreAccount
//!   "conv" : id                           -> ConvMeta (head + participants inline)
//!   "msg"  : "{convId}|{padded ts}|{id}"   -> CoreMessage (sender inline) — key sorts by time
//!   "chan" : "{convId}|{channelId}"        -> CoreChannel  (forum channel list)
//!   "capp" : "{convId}|{appId}"            -> CoreConvApp  (installed mini-apps)

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use futures::lock::Mutex;
use crate::storage::Storage;
use crate::i18n::parse_args;
use crate::{CoreAccount, CoreChannel, CoreConvApp, CoreConversation, CoreMessage};

/// Server `getLangPackDiff` result (changed keys + removed keys + new version).
#[derive(Deserialize)]
struct DiffResp {
    version: u32,
    /// Fingerprint of the server's FULL newest pack — compared against our
    /// merged cache to detect same-version/different-content divergence
    /// (absent on older servers ⇒ no verification, today's behavior).
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    strings: Map<String, Value>,
    #[serde(default)]
    deleted: Vec<String>,
}

/// On-device cache of one language's merged strings + the version it's synced to.
#[derive(Serialize, Deserialize, Default)]
struct CachedPack {
    version: u32,
    #[serde(default)]
    strings: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct ConvMeta {
    kind: String,
    title: Option<String>,
    updated_at_ms: i64,
    unread_count: u32,
    #[serde(default)]
    unread_mention: bool,
    participants: Vec<CoreAccount>,
    #[serde(default)]
    is_forum: bool,
    #[serde(default)]
    forum_member_channels: bool,
    #[serde(default)]
    member_add_members: bool,
    #[serde(default)]
    member_edit_info: bool,
    #[serde(default)]
    member_add_bots: bool,
    /// DENORMALIZED newest message, kept current as messages are upserted, so the
    /// dialog list reads it directly instead of scanning EVERY message of EVERY
    /// conversation. `#[serde(default)]` so caches written before this field still
    /// deserialize (they show no preview until the next message refreshes it).
    #[serde(default)]
    last_message: Option<CoreMessage>,
}

/// True if `a` is newer than `b` in the SAME total order the message key sorts by
/// (time, then id) — so the denormalized last-message tracks the same "newest" the
/// key-ordered scan would have picked.
fn is_newer(a: &CoreMessage, b: &CoreMessage) -> bool {
    (a.created_at_ms.max(0), &a.id) > (b.created_at_ms.max(0), &b.id)
}

/// The timeline a message lives on: its forum channel when set, else the
/// conversation's `#all` main timeline. Every keyed read (`messages`/`thread`)
/// and the reconcile scan bucket on this — a channel is a separate timeline,
/// exactly like the clients' view keying (web keys `s.messages` by channelId).
fn timeline(m: &CoreMessage) -> &str {
    m.channel_id.as_deref().unwrap_or(&m.conversation_id)
}

/// Composite message key → BTree/index order == time order; re-putting the same
/// (timeline, ts, id) overwrites (dedup). ts clamped ≥0 so the zero-pad sorts right.
fn msg_key(m: &CoreMessage) -> String {
    format!("{}|{:020}|{}", timeline(m), m.created_at_ms.max(0), m.id)
}

fn ser<T: Serialize>(v: &T) -> Vec<u8> { serde_json::to_vec(v).unwrap_or_default() }
fn de<T: for<'a> Deserialize<'a>>(v: &[u8]) -> Option<T> { serde_json::from_slice(v).ok() }

pub struct Store<S: Storage> {
    pub(crate) store: S,
    /// Serializes read-modify-write MUTATIONS. Native is already serialized by
    /// `pollster::block_on`, but on wasm two overlapping upserts (an optimistic
    /// write + the server echo) can interleave their get→scan→delete→put awaits
    /// and DUPLICATE; this turns each mutation into one atomic critical section.
    /// Pure reads (`messages`/`conversations`) don't take it.
    /// (`pub(crate)` so sibling modules — flags.rs — join the same discipline.)
    pub(crate) write_lock: Mutex<()>,
    /// Active i18n language pack — read SYNCHRONOUSLY by `t`/`plural` (per-string,
    /// hot path), swapped by `set_language`/`load_cached_language`. A std RwLock
    /// (not the async write_lock) so lookups never await; single-threaded on wasm
    /// (never contends), Sync on native. See `i18n.rs`.
    lang: std::sync::RwLock<crate::i18n::LangPack>,
    /// Dev/internal-build marker for the feature-flag engine (`flags.rs`) —
    /// unset flags resolve to their `dev_default` when true. Set once at open
    /// via `set_flags_dev`; never persisted.
    pub(crate) flags_dev: std::sync::atomic::AtomicBool,
}

impl<S: Storage> Store<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            write_lock: Mutex::new(()),
            lang: std::sync::RwLock::new(crate::i18n::LangPack::default()),
            flags_dev: std::sync::atomic::AtomicBool::new(false),
        }
    }

    // ───────────────────────── i18n (cloud language packs) ─────────────────────────

    /// Synchronous lookup for `key` (active language → English base → key), with
    /// `{name}` interpolation from an optional JSON-object args string.
    pub fn t(&self, key: &str, args_json: Option<&str>) -> String {
        self.lang.read().unwrap_or_else(|e| e.into_inner()).t(key, &parse_args(args_json))
    }

    /// Synchronous CLDR-plural lookup: the active language's plural form for `n`,
    /// with `{n}` + args interpolation.
    pub fn plural(&self, key: &str, n: f64, args_json: Option<&str>) -> String {
        self.lang.read().unwrap_or_else(|e| e.into_inner()).plural(key, n, &parse_args(args_json))
    }

    /// The active language as `{lang, version, rtl, strings}` JSON (strings =
    /// English base overlaid by the active language) — handed to the RN runtime
    /// once per language change so guest JS does fast local lookups.
    pub fn active_langpack_json(&self) -> String {
        let lp = self.lang.read().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({
            "lang": lp.active_lang(),
            "version": lp.active_version(),
            "rtl": lp.rtl(),
            "strings": Value::Object(lp.effective()),
        })
        .to_string()
    }

    /// The active language code (e.g. "zh-Hans" / "en").
    pub fn active_language(&self) -> String {
        self.lang.read().unwrap_or_else(|e| e.into_inner()).active_lang().to_string()
    }

    /// Switch the active language: sync the English base + the chosen language from
    /// the server (delta since our cached version), persist both, swap the in-memory
    /// pack. English is always loaded as the fallback base.
    pub async fn set_language(&self, base: &str, token: &str, code: &str) -> Result<(), String> {
        // The CHOICE is persisted first and unconditionally: a sync that dies
        // half-way must not also lose which language the user picked, or the next
        // open silently re-hydrates the wrong one.
        self.store.put("i18n", "active", code.as_bytes().to_vec()).await;

        // Fetch BOTH tiers before giving up on either. This used to be two `?`
        // early-returns, which meant a half-reachable network (en 200, zh 503)
        // applied NOTHING — the pack stayed empty and every surface painted raw
        // `a.b.c` keys, even though one tier had arrived intact. Whatever we got
        // goes in; whatever we missed falls back to the on-device cache of the
        // same cloud pack; the error is reported so the host can retry.
        let en = self.sync_pack(base, token, "en").await;
        let active = match code {
            "en" => None,
            _ => Some(self.sync_pack(base, token, code).await),
        };

        // Cache reads happen HERE, before the lock: `lang` is a std RwLock whose
        // guard must never be held across an `.await` (it isn't Send, and holding
        // it would block every synchronous `t()` on I/O).
        let en_cached = match &en {
            Err(_) => Some(self.cached_pack("en").await),
            Ok(_) => None,
        };
        let active_cached = match &active {
            Some(Err(_)) => Some(self.cached_pack(code).await),
            _ => None,
        };

        {
            let mut lp = self.lang.write().unwrap_or_else(|e| e.into_inner());
            match (&en, en_cached) {
                (Ok(p), _) => lp.set_base("en", p.strings.clone()),
                // Network lost: fall back to the on-device cache of the same
                // cloud pack rather than blanking one we already had.
                (Err(_), Some(c)) if !lp.is_loaded() && !c.strings.is_empty() => {
                    lp.set_base("en", c.strings)
                }
                (Err(_), _) => {}
            }
            // rtl=false for v0 (en/zh both LTR); RTL metadata comes from
            // resolveLangPacks/listLanguages when an RTL language is added.
            match (&en, &active, active_cached) {
                (_, Some(Ok(p)), _) => lp.set_active(code, p.version, p.strings.clone(), false),
                (_, Some(Err(_)), Some(c)) if !c.strings.is_empty() => {
                    lp.set_active(code, c.version, c.strings, false)
                }
                (Ok(p), None, _) => lp.set_active("en", p.version, Map::new(), false),
                _ => {}
            }
        }

        match (en, active) {
            (Err(e), _) | (_, Some(Err(e))) => Err(e),
            _ => Ok(()),
        }
    }

    /// Whether the cloud pack is delivered and `t()` resolves real strings. There
    /// is NO bundled baseline by design (one pack, cloud-only), so a host that
    /// paints before this is true paints raw keys — gate the first frame on it.
    pub fn langpack_loaded(&self) -> bool {
        self.lang.read().unwrap_or_else(|e| e.into_inner()).is_loaded()
    }

    /// Hydrate the in-memory pack from the on-device cache (offline-first): the
    /// English base + the last active language. Call on open, before `set_language`
    /// refreshes from the network.
    pub async fn load_cached_language(&self) {
        let en = self.cached_pack("en").await;
        let code = self
            .store
            .get("i18n", "active")
            .await
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_else(|| "en".to_string());
        let (active_strings, active_version) = if code == "en" {
            (Map::new(), en.version)
        } else {
            let p = self.cached_pack(&code).await;
            (p.strings, p.version)
        };
        let mut lp = self.lang.write().unwrap_or_else(|e| e.into_inner());
        lp.set_base("en", en.strings);
        lp.set_active(&code, active_version, active_strings, false);
    }

    async fn cached_pack(&self, lang: &str) -> CachedPack {
        self.store
            .get("i18n", &format!("pack/{lang}"))
            .await
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Fetch `getLangPackDiff` from our cached version, merge changed/deleted,
    /// persist, return the merged pack. Uncached ⇒ `from_version: 0` ⇒ full pack.
    ///
    /// SELF-HEAL: the server fingerprints its full newest pack. If our merged
    /// map hashes differently, our cache diverged from the server's snapshot of
    /// the version we claimed (same version number, different content — e.g. a
    /// seed once replaced a version in place), and no future delta can repair
    /// it; refetch the whole pack once and replace wholesale.
    async fn sync_pack(&self, base: &str, token: &str, lang: &str) -> Result<CachedPack, String> {
        let mut cached = self.cached_pack(lang).await;
        let body =
            serde_json::json!({ "lang_code": lang, "from_version": cached.version }).to_string();
        let resp = crate::net::rpc(base, token, "getLangPackDiff", &body).await?;
        let diff: DiffResp = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        for (k, v) in diff.strings {
            cached.strings.insert(k, v);
        }
        for k in &diff.deleted {
            cached.strings.remove(k);
        }
        cached.version = diff.version;
        if diff
            .checksum
            .is_some_and(|want| want != mafold_types::langpack_checksum(&cached.strings))
        {
            let body = serde_json::json!({ "lang_code": lang, "from_version": 0 }).to_string();
            let resp = crate::net::rpc(base, token, "getLangPackDiff", &body).await?;
            let full: DiffResp = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
            cached = CachedPack { version: full.version, strings: full.strings };
        }
        self.store
            .put("i18n", &format!("pack/{lang}"), ser(&cached))
            .await;
        Ok(cached)
    }

    pub async fn upsert_account(&self, a: &CoreAccount) {
        let _g = self.write_lock.lock().await;
        self.store.put("acct", &a.username, ser(a)).await;
    }

    /// Upsert by id; keeps the existing `payload` when the incoming one is None
    /// (mirrors the old `COALESCE(excluded.payload, messages.payload)`).
    ///
    /// Also does OPTIMISTIC-SEND RECONCILE: when the incoming message carries a
    /// `client_msg_id`, any already-stored message with the SAME `client_msg_id`
    /// but a DIFFERENT `id` is the local optimistic placeholder (temp id + local
    /// ts → a different composite key); it's removed so the server echo REPLACES
    /// it instead of duplicating. This dedup rule used to be re-implemented in
    /// both Swift and the web store — now it lives once, in the core.
    pub async fn upsert_message(&self, m: &CoreMessage) {
        let _g = self.write_lock.lock().await;
        self.upsert_message_locked(m, true).await;
    }

    /// The read-modify-write body of `upsert_message`, run INSIDE `write_lock`.
    /// `reconcile` runs the optimistic-send scan (skipped by `replace_messages`,
    /// which just cleared the window — there's nothing to reconcile against, and
    /// scanning per insert is what made the batch O(n²)).
    async fn upsert_message_locked(&self, m: &CoreMessage, reconcile: bool) {
        let key = msg_key(m);
        let mut m = m.clone();
        if m.payload.is_none() {
            if let Some(prev) = self.store.get("msg", &key).await.and_then(|v| de::<CoreMessage>(&v)) {
                m.payload = prev.payload;
            }
        }
        if reconcile {
            if let Some(cid) = m.client_msg_id.clone() {
                // Scan the message's TIMELINE (channel bucket or #all) — the
                // optimistic placeholder was keyed there too.
                let prefix = format!("{}|", timeline(&m));
                for (k, v) in self.store.scan_prefix("msg", &prefix).await {
                    if k == key { continue; }
                    let Some(prev) = de::<CoreMessage>(&v) else { continue };
                    if prev.client_msg_id.as_deref() == Some(cid.as_str()) && prev.id != m.id {
                        // Carry the placeholder's payload forward if the echo lacks
                        // one (same COALESCE spirit — don't drop attachments shown
                        // optimistically before the server's record catches up).
                        if m.payload.is_none() { m.payload = prev.payload; }
                        self.store.delete("msg", &k).await;
                    }
                }
            }
        }
        self.store.put("msg", &key, ser(&m)).await;
        self.bump_last_message(&m).await;
    }

    /// Keep `ConvMeta.last_message` current as messages arrive — denormalizing the
    /// dialog preview so `conversations()` never has to scan every message. Updates
    /// when `m` is newer-or-equal to the stored last (or replaces it when the
    /// stored last is the SAME message id — e.g. a re-upsert/finalize of it).
    async fn bump_last_message(&self, m: &CoreMessage) {
        // A threaded reply never bumps the channel's dialog preview (it lives in
        // its thread, not the timeline) — matches `messages()` excluding replies.
        if m.thread_root_id.is_some() { return; }
        let Some(meta_bytes) = self.store.get("conv", &m.conversation_id).await else { return };
        let Some(mut meta) = de::<ConvMeta>(&meta_bytes) else { return };
        let update = match &meta.last_message {
            None => true,
            Some(cur) => cur.id == m.id || is_newer(m, cur),
        };
        if update {
            meta.last_message = Some(m.clone());
            self.store.put("conv", &m.conversation_id, ser(&meta)).await;
        }
    }

    /// Recompute a conversation's denormalized `last_message` from its remaining
    /// cached messages (after a delete/replace may have removed the old one). Scans
    /// ONE conversation — not the dialog-list hot path.
    async fn recompute_last_message(&self, conv: &str) {
        let Some(meta_bytes) = self.store.get("conv", conv).await else { return };
        let Some(mut meta) = de::<ConvMeta>(&meta_bytes) else { return };
        // Key order == time order, so the newest CHANNEL message is the last one
        // that isn't a threaded reply (replies never surface as the dialog preview).
        let last = self.store.scan_prefix("msg", &format!("{conv}|")).await
            .into_iter().rev()
            .find_map(|(_, v)| de::<CoreMessage>(&v).filter(|m| m.thread_root_id.is_none()));
        meta.last_message = last;
        self.store.put("conv", conv, ser(&meta)).await;
    }

    /// Upsert a conversation head (participants inline) + its last message if any.
    pub async fn upsert_conversation(&self, c: &CoreConversation) {
        let _g = self.write_lock.lock().await;
        self.upsert_conversation_locked(c).await;
    }

    async fn upsert_conversation_locked(&self, c: &CoreConversation) {
        // Preserve a denormalized last_message already tracked locally — the conv
        // head from the server may carry a stale/absent preview; the message stream
        // is the source of truth. Seed it from `c.last_message` only when unset.
        let prior = self.store.get("conv", &c.id).await
            .and_then(|v| de::<ConvMeta>(&v))
            .and_then(|meta| meta.last_message);
        let meta = ConvMeta {
            kind: c.kind.clone(),
            title: c.title.clone(),
            updated_at_ms: c.updated_at_ms,
            unread_count: c.unread_count,
            unread_mention: c.unread_mention,
            participants: c.participants.clone(),
            is_forum: c.is_forum,
            forum_member_channels: c.forum_member_channels,
            member_add_members: c.member_add_members,
            member_edit_info: c.member_edit_info,
            member_add_bots: c.member_add_bots,
            last_message: prior.or_else(|| c.last_message.clone()),
        };
        self.store.put("conv", &c.id, ser(&meta)).await;
        if let Some(m) = &c.last_message {
            self.upsert_message_locked(m, true).await;
        }
    }

    /// Full dialog-list reconcile: upsert all, then drop conversations the server
    /// no longer lists. Cached messages are kept.
    pub async fn replace_conversations(&self, convs: &[CoreConversation]) {
        let _g = self.write_lock.lock().await;
        for c in convs {
            self.upsert_conversation_locked(c).await;
        }
        let keep: std::collections::HashSet<&str> = convs.iter().map(|c| c.id.as_str()).collect();
        for (id, _) in self.store.scan_prefix("conv", "").await {
            if !keep.contains(id.as_str()) {
                self.store.delete("conv", &id).await;
            }
        }
    }

    /// Delete specific messages (by id) from a conversation — mirrors a server
    /// `messageDeleted` (delete-for-everyone), so the persisted cache won't
    /// resurrect them on the next reload. Lives in the core so iOS + web share it.
    pub async fn delete_messages(&self, conv: &str, ids: &[String]) {
        let _g = self.write_lock.lock().await;
        let want: std::collections::HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
        for (k, v) in self.store.scan_prefix("msg", &format!("{conv}|")).await {
            if let Some(m) = de::<CoreMessage>(&v) {
                if want.contains(m.id.as_str()) {
                    self.store.delete("msg", &k).await;
                }
            }
        }
        // The deleted message may have been the cached preview — refresh it.
        self.recompute_last_message(conv).await;
    }

    /// Replace a conversation's cached CHANNEL window (delete old, insert new).
    /// Threaded replies (same conv prefix) are PRESERVED: this replaces the
    /// timeline (server history is top-level only), and wiping replies here would
    /// drop an open thread's loaded replies on every history refresh.
    pub async fn replace_messages(&self, conv: &str, msgs: &[CoreMessage]) {
        let _g = self.write_lock.lock().await;
        for (k, v) in self.store.scan_prefix("msg", &format!("{conv}|")).await {
            if de::<CoreMessage>(&v).is_some_and(|m| m.thread_root_id.is_some()) { continue; }
            self.store.delete("msg", &k).await;
        }
        // The window was just cleared, so there's no stored placeholder to
        // reconcile against — skip the per-insert scan (it's what made this O(n²)).
        // Dedup within the batch by client_msg_id, keeping the newest, so two rows
        // sharing a client_msg_id don't both persist.
        let mut last_for_cid: std::collections::HashMap<&str, &CoreMessage> = std::collections::HashMap::new();
        for m in msgs {
            match &m.client_msg_id {
                Some(cid) => {
                    let keep = last_for_cid.get(cid.as_str()).map_or(true, |prev| !is_newer(prev, m));
                    if keep { last_for_cid.insert(cid.as_str(), m); }
                }
                None => self.upsert_message_locked(m, false).await,
            }
        }
        for m in last_for_cid.into_values() {
            self.upsert_message_locked(m, false).await;
        }
        // last_message tracking above is monotonic on inserts but the clear wiped
        // the window first, so the denormalized preview must be rebuilt from what
        // actually remains.
        self.recompute_last_message(conv).await;
    }

    /// The CHANNEL timeline for a conversation, oldest → newest (key order is time
    /// order). Threaded replies (`thread_root_id.is_some()`) are excluded — they
    /// belong to their thread's `thread()` view, never the main timeline; this is
    /// the split both clients used to do by hand, now owned once by the core.
    pub async fn messages(&self, conv: &str) -> Vec<CoreMessage> {
        self.store.scan_prefix("msg", &format!("{conv}|")).await
            .into_iter()
            .filter_map(|(_, v)| de::<CoreMessage>(&v))
            .filter(|m| m.thread_root_id.is_none())
            .collect()
    }

    /// A thread view: the root message followed by its replies, oldest → newest.
    /// The root and every reply are read from the SINGLE store, so the root copy
    /// returned here is the live one — a streaming root's `{% generating %}` card
    /// is superseded the instant the channel copy is upserted, with no per-client
    /// root reconcile. Replies are the messages whose `thread_root_id == root`.
    pub async fn thread(&self, conv: &str, root: &str) -> Vec<CoreMessage> {
        let mut root_msg = None;
        let mut replies = Vec::new();
        for (_, v) in self.store.scan_prefix("msg", &format!("{conv}|")).await {
            let Some(m) = de::<CoreMessage>(&v) else { continue };
            if m.id == root {
                root_msg = Some(m);
            } else if m.thread_root_id.as_deref() == Some(root) {
                replies.push(m); // scan is key-ordered, so replies stay time-ordered
            }
        }
        // Root pinned first (it's the earliest by time, but make it explicit so a
        // clock-skewed reply can never displace it), then replies in time order.
        root_msg.into_iter().chain(replies).collect()
    }

    /// Conversations newest-first, each with participants + last message — the
    /// dialog list's single source of truth. `last_message` is read from the
    /// denormalized `ConvMeta` (kept current on every upsert), so this no longer
    /// pulls the full blob of EVERY message of EVERY conversation.
    pub async fn conversations(&self) -> Vec<CoreConversation> {
        let mut out = Vec::new();
        for (id, v) in self.store.scan_prefix("conv", "").await {
            let Some(meta) = de::<ConvMeta>(&v) else { continue };
            out.push(CoreConversation {
                id,
                kind: meta.kind,
                title: meta.title,
                participants: meta.participants,
                updated_at_ms: meta.updated_at_ms,
                unread_count: meta.unread_count,
                unread_mention: meta.unread_mention,
                is_forum: meta.is_forum,
                forum_member_channels: meta.forum_member_channels,
                member_add_members: meta.member_add_members,
                member_edit_info: meta.member_edit_info,
                member_add_bots: meta.member_add_bots,
                last_message: meta.last_message,
            });
        }
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        out
    }

    // ───────────────────────── per-device UI state ─────────────────────────

    /// Remember (`Some`) or forget (`None` = #all) the forum channel the user
    /// last had open in `conv`, so reopening the forum lands where they left
    /// off. Per-DEVICE UI state in the local KV (like the i18n active
    /// language) — deliberately not a cloud setting.
    pub async fn set_last_channel(&self, conv: &str, channel: Option<&str>) {
        let _g = self.write_lock.lock().await;
        let key = format!("last_channel/{conv}");
        match channel {
            Some(c) => self.store.put("ui", &key, c.as_bytes().to_vec()).await,
            None => self.store.delete("ui", &key).await,
        }
    }

    /// Replace the cached channel list for `conv`. Whole-list reconcile, like
    /// `replace_conversations`: channels the server no longer lists are dropped,
    /// so a deleted channel can't linger in the picker.
    pub async fn replace_channels(&self, conv: &str, channels: &[CoreChannel]) {
        let _g = self.write_lock.lock().await;
        let prefix = format!("{conv}|");
        for ch in channels {
            self.store
                .put("chan", &format!("{prefix}{}", ch.id), ser(ch))
                .await;
        }
        let keep: std::collections::HashSet<&str> = channels.iter().map(|c| c.id.as_str()).collect();
        for (key, _) in self.store.scan_prefix("chan", &prefix).await {
            let id = key.rsplit('|').next().unwrap_or("");
            if !keep.contains(id) {
                self.store.delete("chan", &key).await;
            }
        }
    }

    /// The cached channels of `conv`, in the order the picker wants them:
    /// pinned first, then MOST-RECENTLY-ACTIVE first, `order` only breaking
    /// ties. Same rule as web's `sortChannels` — a cached list that came back
    /// in a different order than a fresh one would make the picker visibly
    /// reshuffle the moment the network answered. Empty = nothing cached yet.
    pub async fn channels(&self, conv: &str) -> Vec<CoreChannel> {
        let mut out: Vec<CoreChannel> = self
            .store
            .scan_prefix("chan", &format!("{conv}|"))
            .await
            .iter()
            .filter_map(|(_, v)| de::<CoreChannel>(v))
            .collect();
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.activity_at_ms.cmp(&a.activity_at_ms))
                .then(a.order.cmp(&b.order))
        });
        out
    }

    /// Replace the cached mini-app list for `conv` (same reconcile contract).
    pub async fn replace_conv_apps(&self, conv: &str, apps: &[CoreConvApp]) {
        let _g = self.write_lock.lock().await;
        let prefix = format!("{conv}|");
        for a in apps {
            self.store
                .put("capp", &format!("{prefix}{}", a.app_id), ser(a))
                .await;
        }
        let keep: std::collections::HashSet<&str> = apps.iter().map(|a| a.app_id.as_str()).collect();
        for (key, _) in self.store.scan_prefix("capp", &prefix).await {
            let id = key.strip_prefix(&prefix).unwrap_or("");
            if !keep.contains(id) {
                self.store.delete("capp", &key).await;
            }
        }
    }

    /// The cached mini-apps of `conv`, in the server's listed order.
    pub async fn conv_apps(&self, conv: &str) -> Vec<CoreConvApp> {
        let mut out: Vec<CoreConvApp> = self
            .store
            .scan_prefix("capp", &format!("{conv}|"))
            .await
            .iter()
            .filter_map(|(_, v)| de::<CoreConvApp>(v))
            .collect();
        out.sort_by_key(|a| a.order);
        out
    }

    /// The channel last open in `conv` — `None` = #all (never set / cleared).
    pub async fn last_channel(&self, conv: &str) -> Option<String> {
        self.store
            .get("ui", &format!("last_channel/{conv}"))
            .await
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStorage {
        data: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    }
    impl Storage for MemStorage {
        async fn get(&self, t: &str, k: &str) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(&(t.into(), k.into())).cloned()
        }
        async fn put(&self, t: &str, k: &str, v: Vec<u8>) {
            self.data.lock().unwrap().insert((t.into(), k.into()), v);
        }
        async fn delete(&self, t: &str, k: &str) {
            self.data.lock().unwrap().remove(&(t.into(), k.into()));
        }
        async fn scan_prefix(&self, t: &str, p: &str) -> Vec<(String, Vec<u8>)> {
            self.data.lock().unwrap().iter()
                .filter(|((tt, k), _)| tt == t && k.starts_with(p))
                .map(|((_, k), v)| (k.clone(), v.clone())).collect()
        }
    }

    fn store() -> Store<MemStorage> {
        Store::new(MemStorage::default())
    }

    fn acct() -> crate::CoreAccount {
        crate::CoreAccount {
            username: "ops".into(), display_name: "Ops".into(), kind: "human".into(),
            avatar: None, parent_username: None, template: None, language: None,
            verified: false,
        }
    }

    fn msg(id: &str, conv: &str, channel: Option<&str>, ts: i64, cid: Option<&str>) -> CoreMessage {
        CoreMessage {
            id: id.into(), conversation_id: conv.into(), sender: acct(),
            content: format!("m-{id}"), created_at_ms: ts, finalized_at_ms: Some(ts),
            client_msg_id: cid.map(String::from), thread_root_id: None,
            channel_id: channel.map(String::from), payload: None,
        }
    }

    // A channel message lives in ITS timeline bucket — invisible to the #all
    // main timeline, visible under the channel id (mirrors the web view keying).
    #[test]
    fn channel_messages_bucket_separately() {
        let s = store();
        pollster::block_on(async {
            s.upsert_message(&msg("a1", "conv1", None, 100, None)).await;
            s.upsert_message(&msg("c1", "conv1", Some("chanX"), 200, None)).await;
            let main: Vec<String> = s.messages("conv1").await.into_iter().map(|m| m.id).collect();
            let chan: Vec<String> = s.messages("chanX").await.into_iter().map(|m| m.id).collect();
            assert_eq!(main, vec!["a1"], "#all must not leak channel messages");
            assert_eq!(chan, vec!["c1"], "channel timeline reads by channel id");
        });
    }

    // Optimistic-send reconcile works INSIDE a channel bucket: the server echo
    // (same client_msg_id, real id) replaces the optimistic placeholder.
    #[test]
    fn channel_reconcile_replaces_optimistic() {
        let s = store();
        pollster::block_on(async {
            let mut optimistic = msg("local-1", "conv1", Some("chanX"), 100, Some("cmid-1"));
            optimistic.payload = Some("{\"full\":\"local\"}".into());
            s.upsert_message(&optimistic).await;
            // Echo: server id + later ts, same client_msg_id, no payload → must
            // replace the placeholder AND inherit its payload (COALESCE spirit).
            s.upsert_message(&msg("srv-9", "conv1", Some("chanX"), 150, Some("cmid-1"))).await;
            let chan = s.messages("chanX").await;
            assert_eq!(chan.len(), 1, "echo must replace the optimistic copy, not duplicate");
            assert_eq!(chan[0].id, "srv-9");
            assert_eq!(chan[0].payload.as_deref(), Some("{\"full\":\"local\"}"));
        });
    }

    // The same reconcile on the #all timeline (regression pin for the invariant
    // the clients rely on — one implementation, in the core).
    #[test]
    fn main_timeline_reconcile_replaces_optimistic() {
        let s = store();
        pollster::block_on(async {
            s.upsert_message(&msg("local-2", "conv2", None, 100, Some("cmid-2"))).await;
            s.upsert_message(&msg("srv-2", "conv2", None, 160, Some("cmid-2"))).await;
            let ids: Vec<String> = s.messages("conv2").await.into_iter().map(|m| m.id).collect();
            assert_eq!(ids, vec!["srv-2"]);
        });
    }

    // Cross-bucket isolation: identical client_msg_id in ANOTHER channel is a
    // different timeline — reconcile must not reach across buckets.
    #[test]
    fn reconcile_does_not_cross_buckets() {
        let s = store();
        pollster::block_on(async {
            s.upsert_message(&msg("x1", "conv3", Some("chanA"), 100, Some("cmid-3"))).await;
            s.upsert_message(&msg("x2", "conv3", Some("chanB"), 150, Some("cmid-3"))).await;
            assert_eq!(s.messages("chanA").await.len(), 1);
            assert_eq!(s.messages("chanB").await.len(), 1);
        });
    }

    // ── replace_messages batch dedup ──

    /// A write-through page can contain BOTH the optimistic placeholder and the
    /// server echo (same client_msg_id): exactly one row (the newest) survives.
    #[test]
    fn replace_messages_dedups_batch_by_cmid() {
        let s = store();
        pollster::block_on(async {
            let batch = vec![
                msg("local-1", "conv1", None, 100, Some("cmid-1")), // placeholder
                msg("srv-1", "conv1", None, 150, Some("cmid-1")),   // echo — newest, wins
                msg("plain", "conv1", None, 120, None),
            ];
            s.replace_messages("conv1", &batch).await;
            // Assert via messages() (key-ordered) — dedup winners are written in
            // HashMap order, so write order must never be relied on.
            let ids: Vec<String> = s.messages("conv1").await.into_iter().map(|m| m.id).collect();
            assert_eq!(ids, vec!["plain", "srv-1"]);
        });
    }

    #[test]
    fn replace_messages_cmid_tie_keeps_newest_by_id() {
        let s = store();
        pollster::block_on(async {
            // Same timestamp — is_newer tie-breaks on id ("b" > "a").
            let batch = vec![
                msg("a", "conv1", None, 100, Some("cid")),
                msg("b", "conv1", None, 100, Some("cid")),
            ];
            s.replace_messages("conv1", &batch).await;
            let ids: Vec<String> = s.messages("conv1").await.into_iter().map(|m| m.id).collect();
            assert_eq!(ids, vec!["b"]);
        });
    }

    // ── delete_messages → dialog preview recompute ──

    #[test]
    fn delete_messages_recomputes_preview() {
        let s = store();
        pollster::block_on(async {
            s.upsert_conversation(&crate::CoreConversation {
                id: "convP".into(), kind: "direct".into(), title: None,
                participants: vec![acct()], updated_at_ms: 0, unread_count: 0,
                unread_mention: false,
                last_message: None, is_forum: false, forum_member_channels: false,
                member_add_members: false, member_edit_info: false, member_add_bots: false,
            }).await;
            s.upsert_message(&msg("a", "convP", None, 10, None)).await;
            s.upsert_message(&msg("b", "convP", None, 20, None)).await;
            // A thread reply is NEVER the preview, before or after deletes.
            let mut reply = msg("r", "convP", None, 30, None);
            reply.thread_root_id = Some("a".into());
            s.upsert_message(&reply).await;

            let last = |s: &Store<MemStorage>| {
                pollster::block_on(s.conversations())
                    .into_iter().find(|c| c.id == "convP").unwrap()
                    .last_message.map(|m| m.id)
            };
            assert_eq!(last(&s), Some("b".into()));
            s.delete_messages("convP", &["b".into()]).await;
            assert_eq!(last(&s), Some("a".into()), "preview must fall back to the previous message");
            s.delete_messages("convP", &["a".into()]).await;
            assert_eq!(last(&s), None, "no channel messages left → no preview (reply doesn't count)");
        });
    }

    // ── langpack cache + sync (offline / mock network) ──

    #[test]
    fn load_cached_language_hydrates_offline() {
        let s = store();
        pollster::block_on(async {
            // Pre-seed the on-device cache exactly as sync_pack persists it.
            let en = CachedPack { version: 3, strings: serde_json::from_str(r#"{"settings.title":"Settings"}"#).unwrap() };
            let zh = CachedPack { version: 3, strings: serde_json::from_str(r#"{"settings.title":"设置"}"#).unwrap() };
            s.store.put("i18n", "pack/en", ser(&en)).await;
            s.store.put("i18n", "pack/zh-Hans", ser(&zh)).await;
            s.store.put("i18n", "active", b"zh-Hans".to_vec()).await;

            s.load_cached_language().await;
            assert_eq!(s.active_language(), "zh-Hans");
            assert_eq!(s.t("settings.title", None), "设置");
            // Base fallback still works for keys the active pack lacks.
            assert_eq!(s.t("missing.key", None), "missing.key");
        });
        // A FRESH store with no cache falls back to en + key-echo.
        let empty = store();
        pollster::block_on(async {
            empty.load_cached_language().await;
            assert_eq!(empty.active_language(), "en");
            assert_eq!(empty.t("settings.title", None), "settings.title");
        });
    }

    #[tokio::test]
    async fn set_language_syncs_via_mock() {
        use crate::testutil::{ok, spawn_mock};
        let s = store();
        // set_language syncs en FIRST, then the active code — two envelopes.
        let mock = spawn_mock(vec![
            ok(r#"{"version":15,"strings":{"settings.title":"Settings","hello":"Hello {name}"}}"#),
            ok(r#"{"version":15,"strings":{"settings.title":"设置"}}"#),
            // second call: both diffs empty (already current)
            ok(r#"{"version":15,"strings":{}}"#),
            ok(r#"{"version":15,"strings":{}}"#),
        ]);
        s.set_language(&mock.base, "tok", "zh-Hans").await.expect("set_language");
        assert_eq!(s.active_language(), "zh-Hans");
        assert_eq!(s.t("settings.title", None), "设置");
        assert_eq!(s.t("hello", Some(r#"{"name":"Ops"}"#)), "Hello Ops", "en base + interpolation");
        // First requests carried from_version 0 (nothing cached).
        let b0: serde_json::Value = serde_json::from_str(&mock.request(0).body).unwrap();
        assert_eq!(b0["lang_code"], "en");
        assert_eq!(b0["from_version"], 0);

        // Second set_language: the DELTA path — from_version must now be 15,
        // and empty diffs must not lose any strings.
        s.set_language(&mock.base, "tok", "zh-Hans").await.expect("set_language 2");
        let b2: serde_json::Value = serde_json::from_str(&mock.request(2).body).unwrap();
        assert_eq!(b2["from_version"], 15, "cached version must drive the delta request");
        assert_eq!(s.t("settings.title", None), "设置", "empty delta must keep merged strings");
    }

    #[tokio::test]
    async fn set_language_partial_failure_keeps_the_tier_that_arrived() {
        use crate::testutil::{ok, spawn_mock};
        let s = store();
        // en 200, the active language 503 — the half-reachable network that used
        // to `?` out of set_language and leave the pack EMPTY (every surface then
        // painted raw `a.b.c` keys even though en had arrived intact).
        let mock = spawn_mock(vec![
            ok(r#"{"version":15,"strings":{"settings.title":"Settings"}}"#),
            (503, "{\"ok\":false,\"error\":\"upstream\"}".into()),
        ]);
        let res = s.set_language(&mock.base, "tok", "zh-Hans").await;
        assert!(res.is_err(), "the failure must still be reported so the host retries");
        assert_eq!(
            s.t("settings.title", None),
            "Settings",
            "the tier that arrived must be applied, NOT discarded"
        );
        assert!(s.langpack_loaded(), "en-only is still a usable pack, not a blank one");
        // And the user's pick survives the failed sync for the next attempt.
        assert_eq!(
            s.store.get("i18n", "active").await.map(|b| String::from_utf8(b).unwrap()),
            Some("zh-Hans".into()),
        );
    }

    #[tokio::test]
    async fn set_language_falls_back_to_cache_when_the_network_is_gone() {
        use crate::testutil::spawn_mock;
        let s = store();
        let zh = CachedPack {
            version: 9,
            strings: serde_json::from_str(r#"{"settings.title":"设置"}"#).unwrap(),
        };
        let en = CachedPack {
            version: 9,
            strings: serde_json::from_str(r#"{"settings.title":"Settings"}"#).unwrap(),
        };
        s.store.put("i18n", "pack/en", ser(&en)).await;
        s.store.put("i18n", "pack/zh-Hans", ser(&zh)).await;
        // Both tiers fail: the cached CLOUD pack (not a bundled baseline) carries
        // the session; a totally offline open must never paint raw keys.
        let mock = spawn_mock(vec![(503, "{\"ok\":false,\"error\":\"down\"}".into())]);
        assert!(s.set_language(&mock.base, "tok", "zh-Hans").await.is_err());
        assert_eq!(s.t("settings.title", None), "设置");
        assert!(s.langpack_loaded());
    }

    #[test]
    fn langpack_loaded_is_false_until_the_cloud_pack_lands() {
        let s = store();
        assert!(!s.langpack_loaded(), "a fresh store has no pack — hosts must not paint yet");
        pollster::block_on(async {
            let en = CachedPack {
                version: 1,
                strings: serde_json::from_str(r#"{"settings.title":"Settings"}"#).unwrap(),
            };
            s.store.put("i18n", "pack/en", ser(&en)).await;
            s.load_cached_language().await;
        });
        assert!(s.langpack_loaded(), "cached cloud pack ⇒ t() resolves ⇒ safe to paint");
    }

    #[tokio::test]
    async fn sync_pack_applies_deleted_keys() {
        use crate::testutil::{ok, spawn_mock};
        let s = store();
        // Pre-seed a cached pack, then serve a diff that deletes one key.
        let en = CachedPack { version: 5, strings: serde_json::from_str(r#"{"keep":"K","drop":"D"}"#).unwrap() };
        pollster::block_on(s.store.put("i18n", "pack/en", ser(&en)));
        let mock = spawn_mock(vec![ok(r#"{"version":6,"strings":{"keep":"K2"},"deleted":["drop"]}"#)]);
        s.set_language(&mock.base, "tok", "en").await.expect("sync");
        assert_eq!(s.t("keep", None), "K2");
        assert_eq!(s.t("drop", None), "drop", "deleted key must fall through to key-echo");
        // Persisted cache reflects the merge + new version.
        let cached: CachedPack = serde_json::from_slice(&pollster::block_on(s.store.get("i18n", "pack/en")).unwrap()).unwrap();
        assert_eq!(cached.version, 6);
        assert!(!cached.strings.contains_key("drop"));
    }

    /// The poisoned-cache self-heal: a client cached "v21" WITHOUT a key the
    /// server's v21 snapshot had (same version number, different content — the
    /// linsky raw-key bug). The delta can never resend it, but the checksum
    /// mismatch must trigger ONE full refetch that replaces the cache wholesale.
    #[tokio::test]
    async fn sync_pack_checksum_mismatch_heals_via_full_refetch() {
        use crate::testutil::{ok, spawn_mock};
        let s = store();
        // Poisoned cache: v21 with only ONE key; the server's v21 also had
        // "botCustomize.title", so the 21→22 delta below doesn't resend it.
        let poisoned = CachedPack { version: 21, strings: serde_json::from_str(r#"{"settings.title":"Settings"}"#).unwrap() };
        pollster::block_on(s.store.put("i18n", "pack/en", ser(&poisoned)));

        // The server's TRUE full v22 pack + its checksum.
        let full: Map<String, Value> = serde_json::from_str(
            r#"{"settings.title":"Settings","botCustomize.title":"Customize","wallet.new":"Wallet"}"#,
        )
        .unwrap();
        let sum = mafold_types::langpack_checksum(&full);
        let mock = spawn_mock(vec![
            // delta 21→22: only the genuinely-new key, plus the full-pack checksum
            ok(&format!(r#"{{"version":22,"checksum":"{sum}","strings":{{"wallet.new":"Wallet"}}}}"#)),
            // the healing full refetch (from_version 0)
            ok(&format!(
                r#"{{"version":22,"checksum":"{sum}","strings":{}}}"#,
                serde_json::to_string(&Value::Object(full.clone())).unwrap()
            )),
        ]);
        s.set_language(&mock.base, "tok", "en").await.expect("sync");

        assert_eq!(s.t("botCustomize.title", None), "Customize", "healed key must resolve");
        assert_eq!(s.t("wallet.new", None), "Wallet");
        let b0: serde_json::Value = serde_json::from_str(&mock.request(0).body).unwrap();
        assert_eq!(b0["from_version"], 21);
        let b1: serde_json::Value = serde_json::from_str(&mock.request(1).body).unwrap();
        assert_eq!(b1["from_version"], 0, "mismatch must refetch the FULL pack");
        // Persisted cache is the replaced full pack at the new version.
        let cached: CachedPack = serde_json::from_slice(&s.store.get("i18n", "pack/en").await.unwrap()).unwrap();
        assert_eq!(cached.version, 22);
        assert_eq!(cached.strings.len(), 3);
    }

    /// A matching checksum (and an older server that sends none) must NOT
    /// trigger the extra full refetch.
    #[tokio::test]
    async fn sync_pack_checksum_match_or_absent_skips_refetch() {
        use crate::testutil::{ok, spawn_mock};
        let s = store();
        let full: Map<String, Value> = serde_json::from_str(r#"{"a":"1","b":"2"}"#).unwrap();
        let sum = mafold_types::langpack_checksum(&full);
        let mock = spawn_mock(vec![
            // full pack + matching checksum (fresh client, from_version 0)
            ok(&format!(r#"{{"version":9,"checksum":"{sum}","strings":{{"a":"1","b":"2"}}}}"#)),
            // second sync: empty delta, still matching
            ok(&format!(r#"{{"version":9,"checksum":"{sum}","strings":{{}}}}"#)),
            // third sync against an OLD server: no checksum field at all
            ok(r#"{"version":9,"strings":{}}"#),
        ]);
        s.set_language(&mock.base, "tok", "en").await.expect("sync 1");
        s.set_language(&mock.base, "tok", "en").await.expect("sync 2");
        s.set_language(&mock.base, "tok", "en").await.expect("sync 3");
        assert_eq!(s.t("a", None), "1");
        assert_eq!(
            mock.requests.lock().unwrap().len(),
            3,
            "matching / absent checksums must never add refetch requests"
        );
    }

    // ── last-channel (per-device UI state) ──

    /// set/get/clear roundtrip + per-conversation isolation: a forum's saved
    /// channel never leaks into another conv, overwrite wins, and None deletes
    /// the key (= back to #all, the absent-key default).
    #[test]
    fn last_channel_roundtrip_and_isolation() {
        let s = store();
        pollster::block_on(async {
            assert_eq!(s.last_channel("convA").await, None, "unset ⇒ #all");
            s.set_last_channel("convA", Some("chan1")).await;
            s.set_last_channel("convB", Some("chan2")).await;
            assert_eq!(s.last_channel("convA").await.as_deref(), Some("chan1"));
            assert_eq!(s.last_channel("convB").await.as_deref(), Some("chan2"));
            s.set_last_channel("convA", Some("chan9")).await; // user moved on
            assert_eq!(s.last_channel("convA").await.as_deref(), Some("chan9"));
            s.set_last_channel("convA", None).await; // explicit #general = clear
            assert_eq!(s.last_channel("convA").await, None);
            assert_eq!(s.last_channel("convB").await.as_deref(), Some("chan2"), "other conv untouched");
        });
    }
}
