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

pub mod connections;
pub use connections::{
    provider, ConnectionMeta, ProviderKind, ProviderSpec, SecretField, VaultDevice, VaultRecovery,
    PROVIDERS,
};

// MARK: - Files

/// **The** way an asset appears on this wire — avatars, banners, group photos,
/// message media, thumbnails. There is no url field anywhere, by design
/// (owner's call, 2026-08-12): a url is a location, and locations belong in
/// configuration, not in data. Storing one is how `api.mafold.com` stayed
/// frozen in account rows for months after the bytes moved to a CDN.
///
/// Two ids, splitting Telegram's responsibilities:
///
/// * `id` — the **handle**. Fetch bytes from `<file_base>/<id>`, where
///   `file_base` arrives once per connection in `events.hello`. It is also the
///   object key, which is what makes that template work with no per-file round
///   trip and no re-keying of pre-existing objects.
/// * `unique_id` — the **content identity** (sha256). Answers "same file?"
///   without fetching; never resolves to anything. Empty on rows inherited from
///   before the registry, where the bytes were never in our hands: empty means
///   *unknown*, and must never compare equal to another unknown.
///
/// `w`/`h` ride along so a bubble can reserve the box before the image lands.
/// See `.docs/file-id-v1.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRef {
    pub id: String,
    #[serde(default)]
    pub unique_id: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// The bytes never reached the bucket and only the api host has them —
    /// append `id` to the api origin instead of `file_base`. A property of the
    /// file, not a second addressing scheme.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

impl FileRef {
    /// A bare handle, everything else unknown — what an author has when all it
    /// did was upload something and keep the id.
    pub fn from_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            unique_id: String::new(),
            mime: String::new(),
            size_bytes: 0,
            w: None,
            h: None,
            duration_ms: None,
            filename: None,
            local: false,
        }
    }
}

/// Accept either a full [`FileRef`] or a bare id string.
///
/// Author-facing fields (a bot's inline-result thumbnail) are hand-written
/// JSON, and `"thumb": "<file id>"` is the honest shape for someone who only
/// holds the handle. The API fills in the rest from the registry on the way
/// out, so a reader always sees the complete object either way.
pub fn opt_file_ref<'de, D>(d: D) -> Result<Option<FileRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Bare(String),
        Full(FileRef),
    }
    Ok(match Option::<Either>::deserialize(d)? {
        None => None,
        Some(Either::Bare(s)) if s.trim().is_empty() => None,
        Some(Either::Bare(s)) => Some(FileRef::from_id(s)),
        Some(Either::Full(f)) => Some(f),
    })
}

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
    /// The account's picture — a [`FileRef`], like every other asset on this
    /// wire. Was `avatar_url`; see [`FileRef`] for why there is no url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<FileRef>,
    /// Publication banner (the /@handle cover), ~3:1. Same contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<FileRef>,
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

// MARK: - Inline query

/// What picking an inline result does to the composer. Declared BY THE RESULT,
/// so no client ever branches on which bot produced it: a gif bot's row sends,
/// a document bot's row drops a link into the sentence you were writing.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InlinePick {
    /// Send `content` immediately as the user's own message (Telegram's default).
    #[default]
    Send,
    /// Put `content` in the draft and let the user keep typing around it.
    Insert,
}

/// One candidate row in an inline-query picker.
///
/// `content` is the message body that gets sent or inserted — everything else is
/// presentation for the row. Whatever is picked is sent AS THE USER; the queried
/// bot never appears in the conversation, which is what lets a bot answer an
/// inline query from a group it isn't a member of.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InlineResult {
    /// Stable, content-derived id. Clients de-duplicate on it and use it to keep
    /// the highlighted row from jumping while later keystrokes re-answer.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Result thumbnail — a [`FileRef`], like every asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<FileRef>,
    pub content: String,
    /// Seconds a client MAY reuse this answer for an unchanged query. `None` =
    /// don't cache. Set by whoever answers, because only it knows whether the
    /// underlying data is a fixed command list or a live search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<u32>,
    pub on_pick: InlinePick,
}

/// Longest title a picker row can show before it has to be clipped.
const INLINE_TITLE_CHARS: usize = 80;

impl InlineResult {
    /// A plain message body as a single picker row — its own title, sent on pick.
    pub fn from_body(content: String) -> Self {
        Self {
            id: fnv1a_hex(content.as_bytes()),
            title: inline_first_line(&content),
            description: None,
            thumb: None,
            content,
            cache_ttl: None,
            on_pick: InlinePick::Send,
        }
    }
}

/// The first non-empty line, clipped to a picker row's worth. Clipped by CHARS,
/// never bytes — a CJK title cut mid-codepoint would panic on the slice.
fn inline_first_line(content: &str) -> String {
    let line = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(INLINE_TITLE_CHARS).collect();
    if line.chars().count() > INLINE_TITLE_CHARS {
        out.push('…');
    }
    out
}

impl<'de> Deserialize<'de> for InlineResult {
    /// Accepts the structured object OR a bare string.
    ///
    /// The bare string is what `answerInlineQuery` took before there was a
    /// picker to fill, and daemons are user-installed software that updates on
    /// its own schedule — so an old daemon's answer must degrade to one plain
    /// row, not fail the whole query. Partial objects fill in the same way: a
    /// result carrying only a title still sends something rather than an empty
    /// message.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Obj {
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default, deserialize_with = "opt_file_ref")]
            thumb: Option<FileRef>,
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            cache_ttl: Option<u32>,
            #[serde(default)]
            on_pick: InlinePick,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Bare(String),
            Obj(Obj),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Bare(s) => Self::from_body(s),
            Wire::Obj(o) => {
                let content = o.content.or_else(|| o.title.clone()).unwrap_or_default();
                Self {
                    id: o.id.unwrap_or_else(|| fnv1a_hex(content.as_bytes())),
                    title: o.title.unwrap_or_else(|| inline_first_line(&content)),
                    description: o.description,
                    thumb: o.thumb,
                    content,
                    cache_ttl: o.cache_ttl,
                    on_pick: o.on_pick,
                }
            }
        })
    }
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
    /// Someone @-mentioned the requester inside that unread. The badge then
    /// reads `@` instead of a number — which of them is talking to you outranks
    /// how many. Same walk as `unread_count`, so it obeys the same read marker.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unread_mention: bool,
    /// Group avatar (None for direct chats — clients use the peer's avatar).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<FileRef>,
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
    ///
    /// LEGACY WIRE NAME for `member_perms.create_channels` — shipped clients
    /// (core's `CoreConversation`, iOS) read this spelling. It is a mirror, not
    /// a second source of truth: `Store::set_member_perms` writes both and
    /// nothing else writes either.
    #[serde(default, skip_serializing_if = "is_false")]
    pub forum_member_channels: bool,
    /// What ORDINARY members may do here. Absent = every bit false = managers
    /// only, which is exactly how groups behaved before this field existed.
    #[serde(default, skip_serializing_if = "MemberPerms::is_default")]
    pub member_perms: MemberPerms,
}

/// The member-side half of group permissions: powers an ordinary member does
/// NOT have unless the group grants them. The owner and admins always have all
/// of these — these bits only ever widen the circle, never narrow it, so a
/// manager can't be locked out of their own group by a permission toggle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberPerms {
    /// Add other people (`addChatMembers`) and mint invite links.
    #[serde(default)]
    pub add_members: bool,
    /// Edit the group's title / photo / description.
    #[serde(default)]
    pub edit_info: bool,
    /// Add a bot they own to the group (`addGroupBot`).
    #[serde(default)]
    pub add_bots: bool,
    /// Forum only: create channels (Telegram's "Create Topics").
    #[serde(default)]
    pub create_channels: bool,
}

impl MemberPerms {
    /// All-false = the locked-down default; lets the field vanish from the wire
    /// for the overwhelming majority of conversations that never change it.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A shareable invite to a group — the ONLY way to join one without a manager
/// adding you by name.
///
/// A group may hold several at once (one per place you posted it), each
/// independently expirable, capped and revocable, so retiring the link you put
/// in one channel never breaks the others. The code is the whole credential:
/// opaque, never reused, and worthless once `revoked` or spent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteLink {
    /// URL-safe opaque code — the tail of `https://mafold.com/join/<code>`.
    pub code: String,
    pub conversation_id: Uuid,
    /// Who minted it (lowercased username).
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    /// Manager-visible label, so two links are tellable apart ("Twitter", "Q3").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hard expiry. None = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Max successful joins. None = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<u32>,
    /// Successful joins so far. Never decremented — someone leaving does not
    /// refund a seat, or a capped link would be an unbounded one.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub used_count: u32,
    /// Manually killed. Revoked links stay listed (they are the audit trail of
    /// who let whom in) and never work again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub revoked: bool,
}

impl InviteLink {
    /// Usable right now? Mirrors the server's join gate exactly.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        !self.revoked
            && self.expires_at.is_none_or(|t| t > now)
            && self.usage_limit.is_none_or(|n| self.used_count < n)
    }
}

/// What someone holding an invite code sees BEFORE they join — enough to decide,
/// and nothing more. Deliberately not a `Conversation`: a stranger with a link
/// has no business reading the member list or the history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePreview {
    pub conversation_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<FileRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub member_count: u32,
    /// A handful of members for the avatar stack — capped server-side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_members: Vec<Account>,
    /// The caller is already in: the client offers "Open", not "Join".
    #[serde(default, skip_serializing_if = "is_false")]
    pub already_member: bool,
    /// Who minted the link, when that account still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inviter: Option<Account>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

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
    /// Someone @-mentioned the requester inside that unread — the badge reads
    /// `@` instead of a number. Computed in the same walk as `unread_count`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unread_mention: bool,
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
    /// Pinned message ids in THIS channel, newest first.
    ///
    /// A pin belongs to the timeline it was made in. `Conversation::
    /// pinned_message_ids` is therefore the MAIN (`#all`) timeline's list, not
    /// the forum's — before this split, pinning in `#garden` put a bar in every
    /// channel, and since the message isn't in their buckets the bar rendered
    /// with no text. The server routes each pin by the pinned message's own
    /// `channel_id`, so no caller has to say which list it meant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_message_ids: Vec<Uuid>,
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
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_attachments"
    )]
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

fn is_false(b: &bool) -> bool {
    !b
}

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

/// Decode an `attachments` array, DROPPING any element this build no longer
/// understands instead of failing the whole message.
///
/// A message is stored as one blob, and the loader skips anything it cannot
/// parse — so without this, retiring a single attachment variant silently
/// deletes every message that ever carried one. Retiring a shape must cost that
/// attachment, never the message it rode in on.
///
/// It also makes the wire honestly one-directional: an older client can be sent
/// a kind it has never heard of and will show the rest of the message rather
/// than nothing at all.
fn lenient_attachments<'de, D>(d: D) -> Result<Vec<Attachment>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    /// A link-preview card. Kept — unlike the two demo shapes retired beside it,
    /// this one has a real consumer: the Garden brain files it as a URL
    /// candidate (`brains/garden.rs`), including inside merge-forward records.
    News {
        id: String,
        title: String,
        source_id: String,
        url: String,
        snippet: String,
    },
    /// `id` identifies the ATTACHMENT (its slot in this message); `file`
    /// identifies the BYTES. They were conflated before as `id` + `media_id` +
    /// `url` + `w` + `h` — four fields describing one file, kept in sync by
    /// hand. Everything about the file now lives in the one [`FileRef`].
    Photo {
        id: String,
        file: FileRef,
    },
    /// An uploaded video clip (mp4/mov). Mirrors `Photo`; `file.w`/`file.h`
    /// reserve the player box so the bubble doesn't jump on load.
    Video {
        id: String,
        file: FileRef,
    },
    File {
        id: String,
        file: FileRef,
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

impl Attachment {
    /// The attachment's stable id. Every variant carries one, so callers that
    /// need identity (de-dup on append, diffing a message's media) never have
    /// to match per-kind — the whole point of keeping the field uniform.
    pub fn id(&self) -> &str {
        match self {
            Attachment::News { id, .. }
            | Attachment::Photo { id, .. }
            | Attachment::Video { id, .. }
            | Attachment::File { id, .. }
            | Attachment::ChatRecord { id, .. } => id,
        }
    }
}

/// One frozen message inside a `ChatRecord` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordEntry {
    pub sender_name: String,
    pub sender_username: String,
    pub ts: DateTime<Utc>,
    pub content: String,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_attachments"
    )]
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
    pub fn yes() -> Self {
        Self { ok: true }
    }
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
    let mut canonical = String::new();
    canon_json(&serde_json::Value::Object(strings.clone()), &mut canonical);
    fnv1a_hex(canonical.as_bytes())
}

/// JSON rendered with object keys sorted, so two equal documents always produce
/// one string.
///
/// Shared by every checksum in this crate rather than copied per pack: a second
/// canonicaliser that drifted by one comma would make one cache think it was
/// stale forever and another think it was fresh forever, and both failures look
/// like anything but a whitespace difference.
pub(crate) fn canon_json(v: &serde_json::Value, out: &mut String) {
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
                canon_json(&o[k.as_str()], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canon_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// FNV-1a as lowercase hex.
///
/// Spelled out rather than reaching for `DefaultHasher` because these digests
/// cross process AND version boundaries — a seeded langpack's checksum, a
/// client-cached inline result's id — and `DefaultHasher`'s output is explicitly
/// not stable across Rust releases. One that shifted under a toolchain bump
/// would silently invalidate every cache keyed on it.
pub(crate) fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// MARK: - App update (in-app APK distribution for the RN Android client)

/// server → client answer to `appUpdateCheck`. The metadata fields are filled
/// whenever ANY release is live (so the UI can say "you're on the latest,
/// which is X"); `available` alone decides whether an update is offered. `abi`
/// echoes which of the client's ABIs the server matched — pass it back to
/// `appUpdateDownload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUpdateInfo {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_code: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[cfg(test)]
mod inline_result_tests {
    use super::{InlinePick, InlineResult};

    fn parse(json: &str) -> InlineResult {
        serde_json::from_str(json).expect("InlineResult")
    }

    /// THE COMPATIBILITY CASE. Daemons are user-installed and update on their
    /// own schedule, so the pre-picker `Vec<String>` answer must still parse —
    /// as one plain row that is its own title.
    #[test]
    fn a_bare_string_becomes_one_plain_row() {
        let r = parse(r#""看这个 https://example.com""#);
        assert_eq!(r.content, "看这个 https://example.com");
        assert_eq!(r.title, "看这个 https://example.com");
        assert_eq!(r.on_pick, InlinePick::Send);
        assert!(r.description.is_none());
        assert!(!r.id.is_empty());
    }

    /// A whole answer must not fail because one result skipped fields. Title
    /// falls back to the body, and `on_pick` to the Telegram default.
    #[test]
    fn a_partial_object_fills_in_rather_than_erroring() {
        let r = parse(r#"{"content":"周报 2026-W31"}"#);
        assert_eq!(r.title, "周报 2026-W31");
        assert_eq!(r.on_pick, InlinePick::Send);
    }

    /// The inverse: a row that carried only a title still SENDS something. An
    /// empty `content` would post a blank message on pick.
    #[test]
    fn title_only_still_has_a_body_to_send() {
        let r = parse(r#"{"title":"/usage"}"#);
        assert_eq!(r.content, "/usage");
    }

    /// Presentation and payload are separate: the row reads as a page title, the
    /// message that gets inserted is a link.
    #[test]
    fn a_full_object_keeps_content_and_presentation_apart() {
        let r = parse(
            r#"{"id":"pg1","title":"周报模板","description":"Notion · 团队",
                "thumb":"b9OgRzKFL3uZ0MsfuCBkUw","content":"[周报模板](https://notion.so/pg1)",
                "cache_ttl":30,"on_pick":"insert"}"#,
        );
        assert_eq!(r.id, "pg1");
        assert_eq!(r.title, "周报模板");
        assert_eq!(r.description.as_deref(), Some("Notion · 团队"));
        // A bare handle is a valid thumbnail — the author only has the id.
        assert_eq!(r.thumb.as_ref().map(|f| f.id.as_str()), Some("b9OgRzKFL3uZ0MsfuCBkUw"));
        assert_eq!(r.content, "[周报模板](https://notion.so/pg1)");
        assert_eq!(r.cache_ttl, Some(30));
        assert_eq!(r.on_pick, InlinePick::Insert);
    }

    /// A long CJK body must be clipped by CHARS. Slicing bytes at 80 would land
    /// mid-codepoint and panic — the crash this test exists to pin down.
    #[test]
    fn a_long_cjk_title_is_clipped_without_panicking() {
        let body = "中".repeat(200);
        let r = InlineResult::from_body(body);
        assert_eq!(r.title.chars().count(), 81); // 80 + the ellipsis
        assert!(r.title.ends_with('…'));
    }

    /// The title is the first line WITH CONTENT — a body that opens with blank
    /// lines (or a card tag on line 2) must not produce an empty row.
    #[test]
    fn leading_blank_lines_are_skipped_for_the_title() {
        let r = InlineResult::from_body("\n\n  实际标题\n更多正文".into());
        assert_eq!(r.title, "实际标题");
    }

    /// Ids are content-derived and must not drift: clients cache by them.
    #[test]
    fn ids_are_stable_and_content_sensitive() {
        assert_eq!(
            InlineResult::from_body("同样的内容".into()).id,
            InlineResult::from_body("同样的内容".into()).id
        );
        assert_ne!(
            InlineResult::from_body("内容 A".into()).id,
            InlineResult::from_body("内容 B".into()).id
        );
    }
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
