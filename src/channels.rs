//! `mafold channels` — manage a forum's channels from the CLI, so a bot (or a
//! human with a token) can run its own group: list/create/rename/icon/close/
//! pin/archive/delete. Authority is the SERVER's, account-symmetric: managers always,
//! ordinary members when the forum's member-channels toggle is on, and a
//! channel's creator may rename/close their own (Telegram parity).
//!
//! `<chat>` is a conversation id or `@username` (same resolution as `send`);
//! `<channel>` is a channel id or its name (leading `#` optional).

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::client::Client;

#[derive(Subcommand)]
pub enum ChannelsCmd {
    /// List the forum's channels (`#all` is implicit and not listed).
    List { chat: String },
    /// Create a channel.
    Create {
        chat: String,
        name: String,
        /// Emoji icon (e.g. --icon 🐛). Omit for the default "#" tile.
        #[arg(long)]
        icon: Option<String>,
    },
    /// Rename a channel.
    Rename { chat: String, channel: String, name: String },
    /// Set a channel's emoji icon; omit <icon> to clear it.
    Icon { chat: String, channel: String, icon: Option<String> },
    /// Close a channel (read-only lock; history stays, reopen anytime).
    Close { chat: String, channel: String },
    /// Reopen a closed channel.
    Reopen { chat: String, channel: String },
    /// Pin a channel to the top section (cap 5).
    Pin { chat: String, channel: String },
    /// Unpin a channel.
    Unpin { chat: String, channel: String },
    /// Archive a channel (out of the main list into the Archive drawer; it
    /// stays writable — `close` is the read-only lock).
    Archive { chat: String, channel: String },
    /// Bring an archived channel back into the main list.
    Unarchive { chat: String, channel: String },
    /// Delete a channel AND all its contents. Destructive — requires --yes.
    Delete {
        chat: String,
        channel: String,
        /// Confirm the irreversible delete.
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(cmd: ChannelsCmd, client: &Client) -> Result<()> {
    match cmd {
        ChannelsCmd::List { chat } => {
            let chat_id = client.resolve_chat(&chat).await?;
            let channels = fetch(client, &chat_id).await?;
            if channels.is_empty() {
                println!("(no channels — the forum only has its implicit #all timeline)");
                return Ok(());
            }
            for c in &channels {
                let icon = c["icon"].as_str().unwrap_or("#");
                let name = c["name"].as_str().unwrap_or("?");
                let mut marks: Vec<String> = Vec::new();
                if c["pinned"].as_bool() == Some(true) {
                    marks.push("pinned".into());
                }
                if c["closed"].as_bool() == Some(true) {
                    marks.push("closed".into());
                }
                if c["archived"].as_bool() == Some(true) {
                    marks.push("archived".into());
                }
                let unread = c["unread_count"].as_u64().unwrap_or(0);
                if unread > 0 {
                    marks.push(format!("{unread} unread"));
                }
                let suffix = if marks.is_empty() { String::new() } else { format!("  ({})", marks.join(", ")) };
                println!("{icon} {name}{suffix}\n  id: {}", c["id"].as_str().unwrap_or("?"));
            }
        }
        ChannelsCmd::Create { chat, name, icon } => {
            let chat_id = client.resolve_chat(&chat).await?;
            let ch = client.create_channel(&chat_id, &name, icon.as_deref()).await?;
            println!("✓ created #{} (id {})", ch["name"].as_str().unwrap_or(&name), ch["id"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Rename { chat, channel, name } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.edit_channel(&chat_id, &id(&ch)?, Some(&name), None).await?;
            println!("✓ renamed #{} → #{}", ch["name"].as_str().unwrap_or("?"), name);
        }
        ChannelsCmd::Icon { chat, channel, icon } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            // The API contract: Some("") clears, absent = unchanged.
            let icon = icon.unwrap_or_default();
            client.edit_channel(&chat_id, &id(&ch)?, None, Some(&icon)).await?;
            if icon.is_empty() {
                println!("✓ cleared icon on #{}", ch["name"].as_str().unwrap_or("?"));
            } else {
                println!("✓ set icon {icon} on #{}", ch["name"].as_str().unwrap_or("?"));
            }
        }
        ChannelsCmd::Close { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_closed(&chat_id, &id(&ch)?, true).await?;
            println!("✓ closed #{} (read-only; `reopen` to unlock)", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Reopen { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_closed(&chat_id, &id(&ch)?, false).await?;
            println!("✓ reopened #{}", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Pin { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_pinned(&chat_id, &id(&ch)?, true).await?;
            println!("✓ pinned #{}", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Unpin { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_pinned(&chat_id, &id(&ch)?, false).await?;
            println!("✓ unpinned #{}", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Archive { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_archived(&chat_id, &id(&ch)?, true).await?;
            println!("✓ archived #{} (still writable; `unarchive` to bring it back)", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Unarchive { chat, channel } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            client.set_channel_archived(&chat_id, &id(&ch)?, false).await?;
            println!("✓ unarchived #{}", ch["name"].as_str().unwrap_or("?"));
        }
        ChannelsCmd::Delete { chat, channel, yes } => {
            let (chat_id, ch) = resolve(client, &chat, &channel).await?;
            let name = ch["name"].as_str().unwrap_or("?").to_string();
            if !yes {
                bail!("deleting #{name} removes the channel AND all its messages — re-run with --yes to confirm");
            }
            client.delete_channel(&chat_id, &id(&ch)?).await?;
            println!("✓ deleted #{name} and its contents");
        }
    }
    Ok(())
}

fn id(ch: &Value) -> Result<String> {
    Ok(ch["id"].as_str().context("channel has no id")?.to_string())
}

async fn fetch(client: &Client, chat_id: &str) -> Result<Vec<Value>> {
    Ok(client.list_channels(chat_id).await?.as_array().cloned().unwrap_or_default())
}

/// Resolve `<chat>` + `<channel>` (channel id, or name with optional `#`) to
/// (chat_id, channel object). Names match case-insensitively; on a miss the
/// error lists what exists so bots can self-correct.
pub async fn resolve(client: &Client, chat: &str, channel: &str) -> Result<(String, Value)> {
    let chat_id = client.resolve_chat(chat).await?;
    let channels = fetch(client, &chat_id).await?;
    let want = channel.trim().trim_start_matches('#');
    let found = channels
        .iter()
        .find(|c| c["id"].as_str() == Some(want))
        .or_else(|| channels.iter().find(|c| c["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(want))));
    match found {
        Some(c) => Ok((chat_id, c.clone())),
        None => {
            let names: Vec<String> = channels
                .iter()
                .filter_map(|c| c["name"].as_str().map(|n| format!("#{n}")))
                .collect();
            bail!(
                "no channel `{channel}` here — channels: {}",
                if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
            )
        }
    }
}
