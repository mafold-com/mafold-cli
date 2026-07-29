//! Shared wire/protocol model for Mafold — the ONE definition of the data the
//! API serves and clients consume (Account / Conversation / Message / Attachment
//! / Reaction / pagination / auth). Extracted from `mafold-api/src/models.rs` so
//! the backend and the client core can share a single source of truth instead of
//! re-declaring the shapes per side. `mafold-api` re-exports this as its
//! `models` module (so its call sites are unchanged); the client core can adopt
//! it for server-payload (de)serialization.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// MARK: - Account

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Human,
    Bot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub username: String,
    pub display_name: String,
    pub kind: AccountKind,
    /// Plain image URL, stored and served verbatim — the account owner sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Publication banner (the /@handle cover), ~3:1. Same contract as the
    /// avatar: display-ready URL, stored + served verbatim, owner-set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    /// For bot accounts (e.g. `ops:claude`), the username of the human
    /// owner whose API key + cost the bot runs on. None for humans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_username: Option<String>,
    /// For bot accounts, which provider this bot runs against:
    /// `"claude"` / `"deepseek"` / etc. Picks the brain implementation
    /// and the credential row to use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// For external (locally-driven) bots, which agent harness the user's
    /// mafold-cli daemon should drive: `"claude-code"` / `"codex"` /
    /// `"kimi-code"` / `"opencode"` / `"openclaw"`. The daemon reads this from
    /// `getMe` (cloud-first); None ⇒ the daemon's default (claude-code).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Preferred UI language (BCP-47, e.g. "zh-Hans" / "en"). A cloud setting that
    /// syncs across the user's devices; None ⇒ the client uses the device locale.
    /// See .docs/i18n-v0.md.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Platform verification (blue check), granted by ops — first cohort: the
    /// official-AI matrix accounts whose words come from the vendor's official
    /// API. Clients render a badge next to the display name; never self-serve.
    #[serde(default)]
    pub verified: bool,
}

/// A slash command a bot advertises — shown in the chat's command panel when
/// the user types `/`. Telegram-style; each bot defines its own set (via
/// `setBotCommands` with its own token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_hint: Option<String>,
}

// MARK: - Conversation

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConversationKind {
    Direct,
    Group,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub kind: ConversationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub participants: Vec<Account>,
    pub updated_at: DateTime<Utc>,
    /// Telegram-style unread badge count. Server-set; clients render it
    /// as a pill at the end of the dialog row. Zero is omitted by the
    /// `skip_serializing_if`, so the client treats absence as zero.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub unread_count: u32,
    /// Group avatar (None for direct chats — clients use the peer's avatar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Group description / "about" text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Pinned message ids, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_message_ids: Vec<Uuid>,
    /// Pinned installed-app ids (`owner/slug`), newest first. Same manage
    /// gate as message pins; clients surface these ahead of the other
    /// installs (e.g. the launcher's collapsed slots).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_app_ids: Vec<String>,
    /// True when the requesting user has muted this chat. Per-user; filled
    /// in at list time from the caller's mute set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub muted: bool,
    /// True when the requesting user has pinned this chat to the top. Per-user;
    /// filled in at list time from the caller's pin set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// The most recent visible message — populated at list time so clients
    /// render the dialog preview without a per-conversation history fetch.
    /// Not stored; computed per requester (respects their deletes/hidden).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<Message>,
    /// Group creator (lowercased username) — full control. None for direct
    /// chats and for legacy groups created before roles existed (those fall
    /// back to "any participant can manage", preserving prior behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Lowercased usernames the owner has promoted to admin. An admin runs the
    /// group day to day; `transferGroupOwner`, `disbandGroup` and
    /// `setGroupAdmin` stay owner-only, so admins can't appoint, demote or kick
    /// each other.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admins: Vec<String>,
    /// Disbanded: retired group — no sends, no management, but the conversation
    /// and all its messages stay readable in everyone's chat list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disbanded: bool,
    /// True when this group is a forum ("discussion" enabled): its messages are
    /// organized into channels. The main timeline (`#all`, `channel_id = None`)
    /// always exists and needs no migration; extra named channels are `Channel`
    /// records, fetched on demand via `listChannels`. Admin-gated toggle.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_forum: bool,
    /// Forum: may ordinary members create channels? (Telegram's "Create
    /// Topics" member permission.) Default false = managers only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub forum_member_channels: bool,
}

fn is_zero_u32(n: &u32) -> bool { *n == 0 }

/// An extra forum channel (beyond the implicit `#all` main timeline). A group
/// becomes a forum via `is_forum`; each `Channel` is a named sub-timeline whose
/// messages carry `Message.channel_id = Some(this.id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub name: String,
    /// Ascending display order. `#all` is implicit and always sorts first.
    pub order: i32,
    pub created_at: DateTime<Utc>,
    /// Per-requester unread badge, computed at list time (like a dialog row).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub unread_count: u32,
    /// Most recent message in this channel, computed at list time for the
    /// channel-list preview. Not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<Message>,
    /// Closed = read-only lock: history stays, new messages are rejected
    /// server-side (`channel_guard`); reopen anytime. `#all` can't close (v1).
    #[serde(default, skip_serializing_if = "is_false")]
    pub closed: bool,
    /// Pinned to the top section of the channel list (cap 5, Telegram parity).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Archived = out of sight, not read-only: the channel moves out of the main
    /// list into the "Archive" drawer, keeps working (post/read as usual), and
    /// stops contributing to the forum's unread dot. Orthogonal to `closed`
    /// (the read-only lock). Mutually exclusive with `pinned`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
    /// Channel icon: a single emoji. None = the default "#" tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

// MARK: - Message

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub sender: Account,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_id: Option<Uuid>,
    /// Author (username) of the message `reply_to_id` points at. Decorated
    /// server-side at send time — clients never send it. Lets "reply to a
    /// bot's message" engage that bot like an @-mention without any
    /// client-side message-id memory (which a daemon restart would wipe).
    /// `None` on non-replies and on messages stored before this field shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_sender: Option<String>,
    /// Slack-style thread root. `None` = top-level message (lives in the main
    /// channel timeline). `Some(root)` = a thread reply: HIDDEN from the main
    /// timeline, shown only in the thread pane for `root`, and it does NOT bump
    /// the channel's order / last_message / unread. One level deep — replying
    /// in-thread to a reply normalizes to that reply's own root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<Uuid>,
    /// Computed at read time and decorated onto a thread *root* message in the
    /// main timeline. Absent when the message is not a root / has no live
    /// replies. Never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_summary: Option<ThreadSummary>,
    /// Forum channel this message belongs to. `None` = the `#all` main timeline
    /// (the pre-forum default; needs no migration). `Some(ch)` = an extra
    /// `Channel`. Promoted out of the payload — like `thread_root_id` — so the
    /// store partitions channel timelines without parsing content. A message can
    /// carry both `channel_id` (which channel) and `thread_root_id` (a thread
    /// within that channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<DateTime<Utc>>,
    pub reactions: Vec<Reaction>,
    /// Echoed back on `events.messageNew` so clients can match the server
    /// message to their optimistic local copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Set of usernames who have deleted this message **for themselves**
    /// (Telegram "Delete for me"). Filtered out at history-read time so
    /// other participants are unaffected. Never serialised to clients —
    /// they only see whether the message is visible to them.
    #[serde(default, skip)]
    pub hidden_for: HashSet<String>,
    /// True after a "Delete for everyone" — the message survives as a
    /// tombstone (content + attachments cleared) so it can be filtered
    /// out everywhere.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    /// Set when the message has been edited; clients show an "edited" mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<DateTime<Utc>>,
    /// Original author when this message was forwarded from elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from: Option<Account>,
    /// Telegram-style SERVICE message: an in-timeline event (e.g. a minigame
    /// result — "ops 赢得了本局象棋游戏") rendered by clients as a centered
    /// capsule pill with NO sender bubble/avatar. When set, `content` is
    /// typically empty; the pill text lives in `ServiceNotice.text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceNotice>,
}

fn is_false(b: &bool) -> bool { !b }

/// A service-message payload — a ready-to-render centered pill (see
/// `Message::service`). Generic so it's reusable beyond games (member joined,
/// pinned, …): `text` is the display string, `icon` an optional lucide/SF name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceNotice {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

// MARK: - Attachment

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    News {
        id: String,
        title: String,
        source_id: String,
        url: String,
        snippet: String,
    },
    Ticker {
        id: String,
        symbol: String,
        exchange: String,
    },
    Positions {
        id: String,
        account_id: String,
        captured_at: DateTime<Utc>,
    },
    Photo {
        id: String,
        media_id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<u32>,
    },
    /// An uploaded video clip (mp4/mov). Mirrors `Photo` — `w`/`h` reserve the
    /// player box so the bubble doesn't jump on load. Served from `/media` with
    /// Range/seek support.
    Video {
        id: String,
        media_id: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<u32>,
    },
    File {
        id: String,
        media_id: String,
        url: String,
        filename: String,
        size_bytes: u64,
        mime: String,
    },
    /// WeChat-style "merge forward" — a frozen transcript of several messages
    /// bundled into one card. The snapshot is complete (sender + time + content
    /// + each message's own attachments, which may themselves be chat records),
    /// so it survives the originals being edited or deleted.
    ChatRecord {
        id: String,
        /// Human label for the card (source chat name).
        title: String,
        entries: Vec<RecordEntry>,
    },
}

/// One frozen message inside a `ChatRecord` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordEntry {
    pub sender_name: String,
    pub sender_username: String,
    pub ts: DateTime<Utc>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

// MARK: - Report (UGC moderation)

/// A user's report of objectionable content or behavior. Stored for review;
/// required for App Store UGC compliance (Guideline 1.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: Uuid,
    pub reporter: String,
    /// "user" or "message".
    pub target_kind: String,
    /// The reported username or message id.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

// MARK: - Reaction

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    pub message_id: Uuid,
    pub reactor: Account,
    pub emoji: String,
    pub created_at: DateTime<Utc>,
}

// MARK: - Thread summary

/// Per-root thread summary, computed at read time and decorated onto the root
/// `Message` in the main timeline (and informing thread badges on clients).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub root_message_id: Uuid,
    /// Number of live (non-deleted) replies in the thread.
    pub reply_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reply_at: Option<DateTime<Utc>>,
    /// Distinct repliers, newest-first, capped (see `Store::thread_summary`).
    pub participants: Vec<Account>,
    /// Unread replies for the requesting user (replies they didn't send,
    /// after their last thread-read marker).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub unread_count: u32,
}

// MARK: - Pagination wrappers

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesPage {
    pub items: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationsPage {
    pub items: Vec<Conversation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsPage {
    pub items: Vec<Account>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// MARK: - Auth

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    pub access_token: String,
    pub expires_in: i64,
    pub account: Account,
}

// MARK: - Ok placeholder for methods that return nothing meaningful

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ok_ {
    pub ok: bool,
}

impl Ok_ {
    pub fn yes() -> Self { Self { ok: true } }
}

// MARK: - Feature flags (.docs/feature-flags.md)
//
// The wire carries only SERVER-EVALUATED booleans (`FlagState`) — the stable
// seam: richer server targeting never changes the client or this wire shape.
// The compile-time default lives ONLY in the client core's registry; the server
// record (`FlagRecord`) stores a rollout override and deliberately has NO
// default field (invariant ① — one home for the default).

pub type FlagKey = String;

/// server → client: the evaluated flag values for THIS user. Delivered at
/// bootstrap (`getFlags`) and re-pushed over WS (`flagsChanged`) when the
/// control plane changes. Keys absent here fall back to the client default.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlagState {
    pub values: std::collections::BTreeMap<FlagKey, bool>,
    /// Monotonic; a client ignores a delta older than what it already holds.
    pub version: u64,
}

/// How a flag rolls out (the control plane's whole vocabulary — evaluated
/// server-side only; clients never see this, only the resulting bool).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlagRollout {
    /// Force false for everyone.
    Off,
    /// Force true for everyone.
    On,
    /// Deterministic per-user bucket: `hash(account_id + ":" + key) % 100 < p`.
    Percent { p: u8 },
    /// Explicit allowlist (a beta cohort).
    Accounts { list: Vec<String> },
}

/// Control-plane record (admin/storage). A key with NO record ⇒ the client uses
/// its compile-time default (and the server treats server-side gates as open —
/// gating only activates once a record exists).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagRecord {
    pub key: FlagKey,
    pub rollout: FlagRollout,
    /// Admin-facing note; the client-facing description lives in the core registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub updated_at: String,
}

// ── language-pack content fingerprint ────────────────────────────────────────

/// Fingerprint of a language pack's FULL string map — FNV-1a 64 over a
/// canonical (recursively key-sorted) JSON rendering, hex-encoded.
///
/// Both ends of `getLangPackDiff` compute this over the complete latest pack:
/// the server sends it with every delta, and a client whose post-merge map
/// hashes differently has diverged from the server's snapshot of that version
/// (same version number, different content — e.g. a seed replaced a version
/// in place) and must refetch the full pack. Version numbers alone cannot
/// detect that divergence, which otherwise sticks forever.
///
/// Canonicalization is done by hand (objects rendered with sorted keys) so the
/// value is independent of serde_json's `preserve_order` feature, which any
/// crate in a build graph could flip on.
pub fn langpack_checksum(strings: &serde_json::Map<String, serde_json::Value>) -> String {
    fn canon(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String((*k).clone()).to_string());
                    out.push(':');
                    canon(&o[k.as_str()], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(a) => {
                out.push('[');
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    canon(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut canonical = String::new();
    canon(&serde_json::Value::Object(strings.clone()), &mut canonical);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canonical.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod langpack_checksum_tests {
    use super::langpack_checksum;

    fn map(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    /// Pinned vector: the wire value must never drift between releases — an old
    /// client's checksum must keep matching a new server's for identical content.
    #[test]
    fn pinned_vector() {
        let m = map(r#"{"a":"x","n":{"one":"{n} item","other":"{n} items"}}"#);
        assert_eq!(langpack_checksum(&m), "06072a92983c8961");
        assert_eq!(langpack_checksum(&map("{}")), "08f44b07b5901a25");
    }

    /// Key order (top-level AND nested) must not affect the value; content must.
    #[test]
    fn order_independent_content_sensitive() {
        let a = map(r#"{"a":"x","b":{"k1":"1","k2":"2"}}"#);
        let b = map(r#"{"b":{"k2":"2","k1":"1"},"a":"x"}"#);
        assert_eq!(langpack_checksum(&a), langpack_checksum(&b));
        let c = map(r#"{"a":"CHANGED","b":{"k1":"1","k2":"2"}}"#);
        assert_ne!(langpack_checksum(&a), langpack_checksum(&c));
        // A missing key (the poisoned-cache signature) changes the value.
        let d = map(r#"{"a":"x"}"#);
        assert_ne!(langpack_checksum(&a), langpack_checksum(&d));
    }
}
