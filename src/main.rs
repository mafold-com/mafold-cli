//! mafold-cli — Mafold from your terminal.
//!
//!   mafold agent --token mb_… --workdir ~/repo   # run Claude Code as your bot
//!   mafold --token mb_… chats                     # list conversations
//!   mafold --token mb_… send @alice "hi there"    # send a message
//!
//! Auth is a bot token (`mb_…`) via --token or $MAFOLD_BOT_TOKEN.

mod agent;
mod cards;
mod client;
mod commands;
mod daemon;
mod discover;
mod update;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use client::Client;

#[derive(Parser)]
#[command(name = "mafold", version, about = "Mafold from your terminal — CLI client + Claude Code agent")]
struct Cli {
    #[arg(long, env = "MAFOLD_BASE", default_value = "https://api.mafold.com", global = true)]
    base: String,
    #[arg(long, env = "MAFOLD_BOT_TOKEN", global = true)]
    token: Option<String>,
    /// Disable the agent's hourly auto-update.
    #[arg(long, env = "MAFOLD_NO_AUTO_UPDATE", global = true)]
    no_auto_update: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run Claude Code as your bot (daemon): receive messages, reply with the
    /// local `claude` in the working directory.
    Agent {
        #[arg(long, env = "MAFOLD_WORKDIR", default_value = ".")]
        workdir: String,
        /// Run in the background (detached from the terminal) so it keeps
        /// running after you close the shell. Logs to ~/.mafold/agent.log.
        #[arg(long, short)]
        detach: bool,
    },
    /// Stop a background agent started with `agent --detach`.
    Stop,
    /// Show whether a background agent is running.
    Status,
    /// Update mafold to the latest release.
    Update,
    /// List your conversations.
    Chats,
    /// Send a message. <chat> is a conversation id or a @username.
    Send {
        chat: String,
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Author, preview, and publish developer cards.
    Cards {
        #[command(subcommand)]
        cmd: cards::CardsCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Daemon control + self-update need no auth.
    if matches!(cli.cmd, Cmd::Stop) { return daemon::stop(); }
    if matches!(cli.cmd, Cmd::Status) { return daemon::status(); }
    if matches!(cli.cmd, Cmd::Update) {
        let http = reqwest::Client::new();
        match update::update_to_latest(&http).await {
            Ok(Some(v)) => println!("✓ updated to v{v} — restart a running agent with: mafold stop && mafold agent --detach …"),
            Ok(None) => println!("already up to date (v{})", update::current_version()),
            Err(e) => { eprintln!("update failed: {e}"); std::process::exit(1); }
        }
        return Ok(());
    }
    // Cards: init/dev need no token; publish/list check for one themselves.
    if matches!(cli.cmd, Cmd::Cards { .. }) {
        let Cmd::Cards { cmd } = cli.cmd else { unreachable!() };
        return cards::run(cmd, cli.base, cli.token).await;
    }

    let token = cli
        .token
        .context("set --token or $MAFOLD_BOT_TOKEN (your bot's mb_ token — create a bot in the Mafold app)")?;

    match cli.cmd {
        Cmd::Agent { workdir, detach } => {
            // Resolve to an absolute path (default "." = the current folder), so
            // the agent — and the detached child, which has a different cwd —
            // both operate on the same real directory. No fake placeholders.
            let workdir = std::fs::canonicalize(&workdir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(workdir);
            if detach {
                let pid = daemon::start_detached(&cli.base, &token, &workdir)?;
                println!("  workdir: {workdir}");
                println!("✓ agent running in background (pid {pid})");
                println!("  logs:   ~/.mafold/agent.log");
                println!("  status: mafold status");
                println!("  stop:   mafold stop");
            } else {
                agent::run(Client::new(cli.base, token), workdir, !cli.no_auto_update).await?;
            }
        }
        Cmd::Chats => chats(&Client::new(cli.base, token)).await?,
        Cmd::Send { chat, text } => send(&Client::new(cli.base, token), &chat, &text.join(" ")).await?,
        Cmd::Stop | Cmd::Status | Cmd::Update | Cmd::Cards { .. } => unreachable!(),
    }
    Ok(())
}

async fn chats(client: &Client) -> Result<()> {
    let me = client.me().await?;
    let my = me["username"].as_str().unwrap_or_default().to_lowercase();
    let result = client.chats().await?;
    let items = result["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no conversations)");
        return Ok(());
    }
    for c in items {
        let title = c["title"].as_str().map(str::to_string).unwrap_or_else(|| {
            // DM → the other participant's display name.
            c["participants"].as_array().and_then(|ps| {
                ps.iter().find(|p| p["username"].as_str().map(str::to_lowercase) != Some(my.clone()))
            }).and_then(|p| p["display_name"].as_str()).unwrap_or("Chat").to_string()
        });
        let preview = c["last_message"]["content"].as_str().unwrap_or("—");
        let unread = c["unread_count"].as_u64().unwrap_or(0);
        let badge = if unread > 0 { format!("  ({unread})") } else { String::new() };
        let oneline = preview.replace('\n', " ");
        let oneline = if oneline.chars().count() > 60 {
            format!("{}…", oneline.chars().take(60).collect::<String>())
        } else {
            oneline
        };
        println!("• {title}{badge}\n  {oneline}");
    }
    Ok(())
}

async fn send(client: &Client, chat: &str, text: &str) -> Result<()> {
    let chat_id = client.resolve_chat(chat).await?;
    client.send(&chat_id, text).await?;
    println!("✓ sent to {chat}");
    Ok(())
}
