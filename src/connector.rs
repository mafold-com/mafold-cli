//! `mafold connector` — the agent's hands on a connector it does not own.
//!
//! The split this backs is in `.docs/connectors-v1.md` §2: the account holding
//! a third-party token (`@notion`) has no model, and the account with a model
//! (`@claude`, i.e. this daemon) holds no token. This CLI is how the second one
//! asks the first for something, without either giving up what makes it safe.
//!
//! ```text
//! mafold connector list
//! mafold connector run notion "search 周报"
//! mafold connector run notion "page https://www.notion.so/…"
//! mafold connector run notion "append <页面链接> 今天的结论:…"
//! ```
//!
//! **No leading slash.** Connectors accept `/search` and `search` alike, but
//! Git Bash on Windows rewrites any argument that starts with `/` into a
//! Windows path — `"/search"` arrives as `C:/Program Files/Git/search`, which
//! the connector doesn't recognise and answers with its menu. That failure is
//! silent, looks like the command was wrong, and costs a debugging session
//! every time. So the documented form is the one that survives every shell.
//!
//! What comes back is exactly the text `@notion` would have printed in chat —
//! no token, no raw API, nothing this process could misuse later. Every call is
//! checked server-side against a grant the credential's OWNER minted by typing
//! `/allow @<this bot>` at the connector; without one, the failure text names
//! the command they need to send, so the agent can hand a person something to
//! act on instead of "I can't".
//!
//! The conversation comes from `MAFOLD_CONV` (exported per-turn by the harness)
//! for the same reason `mafold room` reads it: concurrent turns run different
//! conversations, and a connector answer belongs to the one that asked.

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::json;

use crate::client::Client;

#[derive(Subcommand)]
pub enum ConnectorCmd {
    /// Which connectors are here, and whose credential I may use.
    List {
        #[arg(long, env = "MAFOLD_CONV")]
        conv: String,
    },
    /// Run one connector command on a person's own credential.
    Run {
        /// Connector handle, e.g. `notion`.
        name: String,
        /// The command WITHOUT a leading slash — `search 周报`, `page <url>`.
        /// (`/search` also works, but Git Bash rewrites a leading `/` into a
        /// Windows path and the connector then answers with its menu.)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Whose credential. Only needed when several people are in the chat.
        #[arg(long = "as")]
        as_user: Option<String>,
        #[arg(long, env = "MAFOLD_CONV")]
        conv: String,
    },
}

pub async fn run(cmd: ConnectorCmd, base: String, token: Option<String>) -> Result<()> {
    let client = Client::new(base, token.unwrap_or_default());
    match cmd {
        ConnectorCmd::List { conv } => {
            let v = client
                .call("connectorList", json!({ "conversation_id": conv }))
                .await?;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                // The distinction that decides what the agent should SAY next:
                // nothing installed at all, versus installed but not delegated.
                let known: Vec<String> = v["connectors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c["connector"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if known.is_empty() {
                    println!("(this server has no connectors configured)");
                } else {
                    println!(
                        "(nobody here has delegated one to me yet — they can send \
                         `/allow @<me>` to @{} to change that)",
                        known.join(" / @")
                    );
                }
                return Ok(());
            }
            for it in items {
                let mode = match (it["read"].as_bool(), it["write"].as_bool()) {
                    (_, Some(true)) => "read+write",
                    _ => "read-only",
                };
                println!(
                    "{}   as @{}   {mode}",
                    it["connector"].as_str().unwrap_or("?"),
                    it["user"].as_str().unwrap_or("?"),
                );
            }
        }
        ConnectorCmd::Run { name, command, as_user, conv } => {
            // Joined with spaces: clap already split the shell's words, and the
            // connector parses "first word is the command" out of one line.
            let command = command.join(" ");
            let mut body = json!({
                "conversation_id": conv,
                "connector": name,
                "command": command,
            });
            if let Some(u) = as_user {
                body["user"] = json!(u.trim_start_matches('@'));
            }
            let v = client.call("connectorRun", body).await?;
            match v["text"].as_str() {
                // Print the connector's own words, unedited. They are written to
                // be read by a person, and the agent relaying them verbatim is
                // usually better than the agent summarising them.
                Some(t) => println!("{t}"),
                None => bail!("connectorRun returned no text: {v}"),
            }
        }
    }
    Ok(())
}

/// Per-turn prompt block: which connectors this conversation can actually
/// reach, and on whose credential.
///
/// Dynamic for the same reason the room block is: a static line saying "you can
/// read their Notion" would be a lie in every chat where nobody granted
/// anything, and the model would confidently try and fail. `None` when this
/// server has no connectors at all, so nothing is injected on a node where the
/// feature doesn't exist.
///
/// Best-effort: any error here returns `Err` and the caller drops it — a
/// connector listing is never worth failing a turn over.
pub async fn context_block(client: &Client, conv: &str) -> anyhow::Result<Option<String>> {
    let v = client
        .call("connectorList", json!({ "conversation_id": conv }))
        .await?;
    let known: Vec<String> = v["connectors"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["connector"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if known.is_empty() {
        return Ok(None);
    }
    let items = v["items"].as_array().cloned().unwrap_or_default();
    let mut s = String::from(
        "[CONNECTORS — accounts that hold someone's third-party credentials (@notion, @github). \
You hold none of them and never will; you ask THEM, and they act on the credential of the \
person who authorized you. `mafold connector run <name> \"<command>\"` (conversation preset via \
MAFOLD_CONV) returns exactly what that connector would have said in chat.]\n",
    );
    if items.is_empty() {
        s.push_str(&format!(
            "Available on this server: @{}.\nNobody in this conversation has authorized you yet, \
so every call will be refused. If the user asks for something in one of these, tell them to send \
`/allow @<your handle>` to that connector in a DM (add `write` on the end to let you change \
things too) — then try again. Do not guess at their content in the meantime.\n",
            known.join(", @")
        ));
    } else {
        s.push_str("You may act for:\n");
        for it in &items {
            s.push_str(&format!(
                "• {} as @{} — {}\n",
                it["connector"].as_str().unwrap_or("?"),
                it["user"].as_str().unwrap_or("?"),
                if it["write"].as_bool().unwrap_or(false) {
                    "read + write"
                } else {
                    "READ ONLY (a write will be refused — ask them for `/allow @<you> write`)"
                },
            ));
        }
        s.push_str(
            "Write the command WITHOUT a leading slash (`search 周报`, not `/search 周报`) — some \
shells rewrite a leading `/` into a path and the connector then just prints its menu. Run \
`mafold connector run <name> help` to see that connector's own command list instead of guessing \
at one. A write changes a real document: say what you are about to write and get a yes first — \
a {% mafold/ask %} card is the cheap way to ask.\n",
        );
    }
    s.push_str("[END CONNECTORS]");
    Ok(Some(s))
}
