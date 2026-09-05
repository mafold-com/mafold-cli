//! Adopt the shared `mafold-types` wire model: convert the server's wire shapes
//! (uuid/chrono-typed — the SAME definitions the backend serves) into the core's
//! FFI-friendly types (String ids, i64 epoch-ms). This is the seam the core uses
//! to ingest server REST/WS payloads directly into its store, so there's one
//! definition of the protocol across backend + core.

use mafold_types as wire;
use crate::{CoreAccount, CoreConversation, CoreMessage};

impl From<&wire::Account> for CoreAccount {
    fn from(a: &wire::Account) -> Self {
        CoreAccount {
            username: a.username.clone(),
            display_name: a.display_name.clone(),
            kind: if a.kind == wire::AccountKind::Bot { "bot" } else { "human" }.to_string(),
            avatar: a.avatar.as_ref().map(Into::into),
            parent_username: a.parent_username.clone(),
            template: a.template.clone(),
            language: a.language.clone(),
            verified: a.verified,
        }
    }
}

impl From<&wire::Message> for CoreMessage {
    fn from(m: &wire::Message) -> Self {
        CoreMessage {
            id: m.id.to_string(),
            conversation_id: m.conversation_id.to_string(),
            sender: (&m.sender).into(),
            content: m.content.clone(),
            created_at_ms: m.created_at.timestamp_millis(),
            finalized_at_ms: m.finalized_at.map(|d| d.timestamp_millis()),
            client_msg_id: m.client_msg_id.clone(),
            thread_root_id: m.thread_root_id.map(|u| u.to_string()),
            channel_id: m.channel_id.map(|u| u.to_string()),
            // Keep the full wire message as the opaque payload so the client
            // rehydrates faithfully (attachments / reactions / reply / thread).
            payload: serde_json::to_string(m).ok(),
        }
    }
}

impl From<&wire::Conversation> for CoreConversation {
    fn from(c: &wire::Conversation) -> Self {
        CoreConversation {
            id: c.id.to_string(),
            kind: if c.kind == wire::ConversationKind::Group { "group" } else { "direct" }.to_string(),
            title: c.title.clone(),
            participants: c.participants.iter().map(CoreAccount::from).collect(),
            updated_at_ms: c.updated_at.timestamp_millis(),
            unread_count: c.unread_count,
            unread_mention: c.unread_mention,
            is_forum: c.is_forum,
            forum_member_channels: c.forum_member_channels,
            member_add_members: c.member_perms.add_members,
            member_edit_info: c.member_perms.edit_info,
            member_add_bots: c.member_perms.add_bots,
            last_message: c.last_message.as_ref().map(CoreMessage::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{CoreConversation, CoreMessage};
    use mafold_types as wire;

    #[test]
    fn wire_conversation_converts_to_core() {
        let c: wire::Conversation = serde_json::from_str(
            r#"{
                "id": "0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
                "kind": "group",
                "title": "Core Forum",
                "participants": [
                    {"username":"ops","display_name":"Ops","kind":"human"},
                    {"username":"mafold:ai","display_name":"Mafold AI","kind":"bot","parent_username":"mafold"}
                ],
                "updated_at": "2026-07-14T00:00:01Z",
                "unread_count": 3,
                "is_forum": true,
                "forum_member_channels": true,
                "last_message": {
                    "id": "6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5",
                    "conversation_id": "0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
                    "sender": {"username":"ops","display_name":"Ops","kind":"human"},
                    "content": "hi",
                    "created_at": "2026-07-14T00:00:00Z",
                    "reactions": []
                }
            }"#,
        )
        .unwrap();
        let core = CoreConversation::from(&c);
        assert_eq!(core.id, "0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1");
        assert_eq!(core.kind, "group");
        assert_eq!(core.title.as_deref(), Some("Core Forum"));
        assert_eq!(core.participants.len(), 2);
        assert_eq!(core.participants[1].kind, "bot", "AccountKind::Bot must map to \"bot\"");
        assert_eq!(core.participants[1].parent_username.as_deref(), Some("mafold"));
        assert_eq!(core.unread_count, 3);
        assert!(core.is_forum && core.forum_member_channels, "forum flags must survive (regression: wire.rs once hardcoded false)");
        assert_eq!(core.updated_at_ms, 1_783_987_201_000, "chrono → epoch-ms (2026-07-14T00:00:01Z)");
        let last = core.last_message.expect("nested last_message converts");
        assert_eq!(last.content, "hi");
        assert!(last.payload.is_some(), "nested message keeps the full-wire payload");
    }

    /// Fields with serde defaults must deserialize when ABSENT and map sanely.
    #[test]
    fn wire_conversation_defaults_and_direct_kind() {
        let c: wire::Conversation = serde_json::from_str(
            r#"{
                "id": "0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
                "kind": "direct",
                "participants": [],
                "updated_at": "2026-07-14T00:00:00Z"
            }"#,
        )
        .unwrap();
        let core = CoreConversation::from(&c);
        assert_eq!(core.kind, "direct");
        assert_eq!(core.unread_count, 0);
        assert!(!core.is_forum && !core.forum_member_channels);
        assert!(core.last_message.is_none());
        // And a channel message stamps channel_id on the core shape.
        let m: wire::Message = serde_json::from_str(
            r#"{
                "id": "6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5",
                "conversation_id": "0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
                "sender": {"username":"ops","display_name":"Ops","kind":"human"},
                "content": "in channel",
                "channel_id": "11111111-2222-3333-4444-555555555555",
                "created_at": "2026-07-14T00:00:00Z",
                "reactions": []
            }"#,
        )
        .unwrap();
        let cm = CoreMessage::from(&m);
        assert_eq!(cm.channel_id.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
    }
}
