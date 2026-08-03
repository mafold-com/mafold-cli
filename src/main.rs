//! mafold-cli — Mafold from your terminal.
//!
//!   mafold agent --token mb_… --workdir ~/repo   # run Claude Code as your bot
//!   mafold --token mb_… chats                     # list conversations
//!   mafold --token mb_… send @alice "hi there"    # send a message
//!
//! Auth is a bot token (`mb_…`) via --token or $MAFOLD_BOT_TOKEN.

mod agent;
mod apps;
mod ask_hook;
mod bash_hook;
mod cards;
mod channels;
mod client;
mod commands;
mod connector;
mod daemon;
mod discover;
mod harness;
mod install;
mod langpack;
mod session;
mod platform;
mod render;
mod room;
mod supervisor;
mod update;
mod wallet;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use client::{Client, Dest};

#[derive(Parser)]
#[command(name = "mafold", version, about = "Mafold from your terminal — CLI client + coding-agent daemon (Claude Code, Codex, …)")]
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
    /// Run an agent harness as your bot (daemon): receive messages, reply with
    /// the local agent CLI in the working directory.
    Agent {
        /// Working directory the harness runs in. If omitted, the bot's
        /// owner-set server config (`cwd`/`workdir`) is used, else the current
        /// directory. An explicit flag always wins over the server config.
        #[arg(long, env = "MAFOLD_WORKDIR")]
        workdir: Option<String>,
        /// Which agent harness to drive: claude-code (default), opencode, codex,
        /// openclaw. (Others land as they're implemented.)
        #[arg(long, env = "MAFOLD_HARNESS", default_value = "claude-code")]
        harness: String,
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
    /// Install a coding-agent runtime (claude-code / codex / kimi-code /
    /// opencode). No argument lists the runtimes + their install state.
    Install {
        /// The runtime to install; omit to list all with their state.
        tool: Option<String>,
        /// Don't ask before running the official installer.
        #[arg(long, short)]
        yes: bool,
    },
    /// List your conversations.
    Chats,
    /// Send a message. <chat> is a conversation id or a @username.
    Send {
        chat: String,
        /// Send into a forum channel (id or #name) instead of the main timeline.
        #[arg(long)]
        channel: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Attach local image files to the reply you are streaming right now.
    /// Run by an AGENT mid-turn (the daemon presets MAFOLD_DRAFT), so a picture
    /// it just made arrives in the same bubble as the text about it.
    Attach {
        /// Image files on this machine.
        #[arg(required = true)]
        files: Vec<String>,
        /// Message to attach to. Defaults to `$MAFOLD_DRAFT` — the in-flight
        /// reply — which is what an agent almost always wants.
        #[arg(long)]
        message: Option<String>,
    },
    /// Manage a forum's channels (list/create/rename/close/pin/delete).
    Channels {
        #[command(subcommand)]
        cmd: channels::ChannelsCmd,
    },
    /// Token wallet: balances / transfer / convert / rates / history / grants.
    Wallet {
        #[command(subcommand)]
        cmd: wallet::WalletCmd,
    },
    /// Author, preview, and publish developer cards.
    Cards {
        #[command(subcommand)]
        cmd: cards::CardsCmd,
    },
    /// Author, preview, and publish developer mini-apps.
    Apps {
        #[command(subcommand)]
        cmd: apps::AppsCmd,
    },
    /// Read/write an app's shared CRDT room in a conversation (the AI's room
    /// peer — backs the `mafold-room` skill). Conversation via `--conv` /
    /// `MAFOLD_CONV`; auth via `--token` / `MAFOLD_BOT_TOKEN`.
    Room {
        #[command(subcommand)]
        cmd: room::RoomCmd,
    },
    /// Ask a connector (@notion, @github) for something on a person's own
    /// credential — the agent side of the no-model/no-credential split. Needs a
    /// grant they minted with `/allow @<this bot>`. Conversation via `--conv` /
    /// `MAFOLD_CONV`; auth via `--token` / `MAFOLD_BOT_TOKEN`.
    Connector {
        #[command(subcommand)]
        cmd: connector::ConnectorCmd,
    },
    /// Publish the cloud language packs (langpacks/*.json) — first-party only.
    Langpack {
        #[command(subcommand)]
        cmd: langpack::LangpackCmd,
    },

    // ── multi-daemon supervisor: one daemon per bot ──
    /// Add a bot daemon to the local config (token from --token).
    Add {
        /// The bot username (also the daemon's pid/log name).
        name: String,
        #[arg(long, env = "MAFOLD_WORKDIR")]
        workdir: String,
        /// Local harness hint; the server (getMe) is authoritative at runtime.
        #[arg(long)]
        harness: Option<String>,
    },
    /// Remove a bot daemon from the local config (stops it too).
    Rm { name: String },
    /// Start the supervisor — keeps all configured daemons running + owns updates.
    Up,
    /// Stop the supervisor + all daemons (or one daemon by name).
    Down { name: Option<String> },
    /// Show the last lines of a bot daemon's log.
    Logs { name: String },

    // ── human control plane (New-Bot harness recommendation + provisioning) ──
    /// Log in your HUMAN account so the Mafold app can recommend a harness from
    /// this machine (and soon auto-provision bots). Reports installed harnesses.
    Login {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// Re-report this machine's available harnesses (uses the saved login).
    Report,
    /// Roll back to the previous binary (after a bad update).
    Rollback,
    /// (internal) The long-lived supervisor loop — started by `up`.
    #[command(hide = true)]
    Supervise {
        /// Hide this process's console window (Windows: the logon task hands a
        /// console binary a visible console at sign-in). Keep this flag forever:
        /// registered tasks reference it, so removing it would make every
        /// existing task fail at logon with a clap error (exit code 2).
        #[arg(long, hide = true)]
        hidden: bool,
    },
    /// (internal) PreToolUse hook claude runs for AskUserQuestion — blocks until
    /// the user answers the chat card, then feeds the answer back. Not for humans.
    #[command(hide = true)]
    AskHook,
    /// (internal) PreToolUse hook claude runs for Bash — detaches
    /// run_in_background tasks into their own session so they survive the turn
    /// (claude kills its background shells at exit). Not for humans.
    #[command(hide = true)]
    BashHook,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The agent stores its pid/log/config under `~/.mafold`, keyed off `$HOME`.
    // Windows doesn't set HOME — fall back to USERPROFILE so the same paths work.
    if std::env::var_os("HOME").is_none() {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            std::env::set_var("HOME", profile);
        }
    }

    let cli = Cli::parse();

    // The AskUserQuestion hook is invoked by claude (a child of the daemon) and
    // needs no auth — it just bridges stdin/the answer file. Handle it first.
    if matches!(cli.cmd, Cmd::AskHook) { return ask_hook::run(); }
    if matches!(cli.cmd, Cmd::BashHook) { return bash_hook::run(); }

    // Daemon control + self-update need no auth.
    if matches!(cli.cmd, Cmd::Stop) { return daemon::stop(); }
    if matches!(cli.cmd, Cmd::Status) {
        let _ = daemon::status(); // legacy single `agent --detach`
        supervisor::status();     // multi-daemon config
        return Ok(());
    }
    match &cli.cmd {
        Cmd::Up => return supervisor::up(&cli.base, cli.no_auto_update),
        Cmd::Down { name } => return supervisor::down(name.as_deref()),
        Cmd::Logs { name } => return supervisor::logs(name),
        Cmd::Rm { name } => return supervisor::rm(name),
        Cmd::Rollback => return update::rollback(),
        Cmd::Supervise { hidden } => {
            if *hidden {
                platform::hide_console();
            }
            supervisor::supervise(cli.base, !cli.no_auto_update).await;
            return Ok(());
        }
        _ => {}
    }
    if matches!(cli.cmd, Cmd::Update) {
        // No release binary is built for this platform (e.g. linux-arm64) → don't
        // claim "up to date" (the check would always no-op). Be honest instead.
        if !update::platform_supported() {
            println!("no mafold release is built for your platform — self-update isn't available.\nSee https://github.com/mafold-lab/mafold-cli/releases");
            return Ok(());
        }
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
    // Apps: init/dev need no token; publish/list/remove check for one themselves.
    if matches!(cli.cmd, Cmd::Apps { .. }) {
        let Cmd::Apps { cmd } = cli.cmd else { unreachable!() };
        return apps::run(cmd, cli.base, cli.token).await;
    }
    // Room: the AI's CRDT room peer. Auth via --token / MAFOLD_BOT_TOKEN.
    if matches!(cli.cmd, Cmd::Room { .. }) {
        let Cmd::Room { cmd } = cli.cmd else { unreachable!() };
        return room::run(cmd, cli.base, cli.token).await;
    }
    // Connector: reach @notion / @github on a person's own credential, via the
    // grant they minted with `/allow`. Bot token — the caller IS the agent.
    if matches!(cli.cmd, Cmd::Connector { .. }) {
        let Cmd::Connector { cmd } = cli.cmd else { unreachable!() };
        return connector::run(cmd, cli.base, cli.token).await;
    }
    // Langpack: publish/list check for the first-party token themselves.
    if matches!(cli.cmd, Cmd::Langpack { .. }) {
        let Cmd::Langpack { cmd } = cli.cmd else { unreachable!() };
        return langpack::run(cmd, cli.base, cli.token).await;
    }
    // Human control plane: `login` mints the s_ session; `report` uses it. No bot token.
    if matches!(cli.cmd, Cmd::Login { .. }) {
        let Cmd::Login { username, password } = cli.cmd else { unreachable!() };
        return login(&cli.base, username, password).await;
    }
    if matches!(cli.cmd, Cmd::Report) {
        return report_harnesses(&cli.base).await;
    }
    // Machine setup — no account or token involved at all.
    if let Cmd::Install { tool, yes } = &cli.cmd {
        return install::run(tool.as_deref().unwrap_or(""), *yes);
    }

    let token = cli
        .token
        .context("set --token or $MAFOLD_BOT_TOKEN (your bot's mb_ token — create a bot in the Mafold app)")?;

    match cli.cmd {
        Cmd::Agent { workdir, harness, detach } => {
            // An explicit --workdir wins; if omitted, the server owner-config (or
            // the current dir) decides at runtime. Resolve an explicit one to an
            // absolute path so the agent — and the detached child, which has a
            // different cwd — both operate on the same real directory.
            let workdir = workdir.map(|w| {
                std::fs::canonicalize(&w).map(|p| p.to_string_lossy().into_owned()).unwrap_or(w)
            });
            if detach {
                let pid = daemon::start_detached(&cli.base, &token, workdir.as_deref(), &harness, cli.no_auto_update)?;
                if let Some(w) = &workdir { println!("  workdir: {w}"); }
                println!("✓ agent running in background (pid {pid})");
                println!("  logs:   ~/.mafold/agent.log");
                println!("  status: mafold status");
                println!("  stop:   mafold stop");
            } else {
                agent::run(Client::new(cli.base, token), workdir, harness, !cli.no_auto_update).await?;
            }
        }
        Cmd::Add { name, workdir, harness } => supervisor::add(name, token, workdir, harness, &cli.base, cli.no_auto_update)?,
        Cmd::Chats => chats(&Client::new(cli.base, token)).await?,
        Cmd::Send { chat, channel, text } => {
            send(&Client::new(cli.base, token), &chat, channel.as_deref(), &text.join(" ")).await?
        }
        Cmd::Attach { files, message } => {
            attach(&Client::new(cli.base, token), &files, message.as_deref()).await?
        }
        Cmd::Channels { cmd } => channels::run(cmd, &Client::new(cli.base, token)).await?,
        Cmd::Wallet { cmd } => wallet::run(cmd, &Client::new(cli.base, token)).await?,
        Cmd::Stop | Cmd::Status | Cmd::Update | Cmd::Install { .. } | Cmd::Cards { .. }
        | Cmd::Apps { .. } | Cmd::Room { .. } | Cmd::Connector { .. }
        | Cmd::Langpack { .. } | Cmd::Login { .. } | Cmd::Report
        | Cmd::Up | Cmd::Down { .. } | Cmd::Logs { .. } | Cmd::Rm { .. }
        | Cmd::Rollback | Cmd::Supervise { .. } | Cmd::AskHook | Cmd::BashHook => unreachable!(),
    }
    Ok(())
}

fn prompt(label: &str) -> String {
    use std::io::Write;
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

/// Read a secret without echoing (Unix: toggle the tty via `stty`).
fn prompt_password(label: &str) -> String {
    use std::io::Write;
    print!("{label}");
    let _ = std::io::stdout().flush();
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    s.trim().to_string()
}

/// `mafold login` — mint a human `s_` session + report this machine's harnesses.
async fn login(base: &str, username: Option<String>, password: Option<String>) -> Result<()> {
    // Default (no args) → browser device flow, à la `gh auth login`. Passing
    // --username/--password keeps the direct password login (CI / scripted).
    if username.is_none() && password.is_none() {
        return login_device(base).await;
    }
    let username = username.unwrap_or_else(|| prompt("Mafold username: "));
    let password = password.unwrap_or_else(|| prompt_password("Password: "));
    let http = reqwest::Client::new();
    let resp: serde_json::Value = http
        .post(format!("{base}/api/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send().await.context("login request failed")?
        .json().await.context("login: non-JSON response")?;
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        anyhow::bail!("login failed: {}", resp["description"].as_str().unwrap_or("check username/password"));
    }
    let result = &resp["result"];
    let token = result["token"].as_str().context("login: no token in response")?.to_string();
    let uname = result["user"]["username"].as_str().unwrap_or(&username).to_string();
    finish_login(base, token, uname).await
}

/// gh-style device login: get a short code, the user approves it in the Mafold
/// web app, and we poll until the session token comes back. Works on headless /
/// remote machines (no browser needed on THIS box — approve from your phone).
async fn login_device(base: &str) -> Result<()> {
    let http = reqwest::Client::new();
    let start: serde_json::Value = http
        .post(format!("{base}/api/auth/device/start"))
        .json(&serde_json::json!({ "device": session::device_name(), "platform": std::env::consts::OS }))
        .send().await.context("device/start failed")?
        .json().await.context("device/start: non-JSON response")?;
    let r = &start["result"];
    let device_code = r["device_code"].as_str().context("device/start: no device_code")?.to_string();
    let user_code = r["user_code"].as_str().unwrap_or("");
    let verify_url = r["verify_url"].as_str().unwrap_or("https://mafold.com/login/device");
    let interval = r["interval"].as_u64().unwrap_or(3).max(1);

    println!("\n  Open this URL in your browser:  {verify_url}");
    println!("  and enter the code:             {user_code}\n");
    println!("  Waiting for you to approve…  (Ctrl-C to cancel)");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let poll: serde_json::Value = http
            .post(format!("{base}/api/auth/device/poll"))
            .json(&serde_json::json!({ "device_code": device_code }))
            .send().await.context("device/poll failed")?
            .json().await.context("device/poll: non-JSON response")?;
        match poll["result"]["status"].as_str().unwrap_or("") {
            "approved" => {
                let token = poll["result"]["token"].as_str().context("approved but no token")?.to_string();
                let uname = poll["result"]["username"].as_str().unwrap_or("").to_string();
                return finish_login(base, token, uname).await;
            }
            "expired" => anyhow::bail!("that code expired — run `mafold login` again"),
            _ => {} // pending — keep polling
        }
    }
}

/// Persist the session + report this machine's harnesses (shared by both paths).
async fn finish_login(base: &str, token: String, uname: String) -> Result<()> {
    let prev = session::load();
    let sess = session::Session {
        token,
        username: uname.clone(),
        device_id: session::device_id(prev.as_ref().map(|p| p.device_id.as_str())),
        device_name: session::device_name(),
    };
    session::save(&sess)?;
    println!("✓ logged in as {uname} on {}", sess.device_name);
    report_with(base, &sess).await?;
    println!("\n→ keep this machine available + auto-provision new bots:  mafold up");
    Ok(())
}

/// `mafold report` — re-report this machine's available harnesses.
async fn report_harnesses(base: &str) -> Result<()> {
    let sess = session::load().context("not logged in — run `mafold login` first")?;
    report_with(base, &sess).await
}

async fn report_with(base: &str, sess: &session::Session) -> Result<()> {
    let probed = harness::probe_with_versions();
    let harnesses: Vec<serde_json::Value> = probed
        .iter()
        .map(|(id, available, version)| serde_json::json!({ "id": id, "available": available, "version": version }))
        .collect();
    let avail: Vec<&str> = probed.iter().filter(|(_, a, _)| *a).map(|(id, _, _)| *id).collect();
    Client::new(base.to_string(), sess.token.clone())
        .call(
            "reportHarnesses",
            serde_json::json!({
                "device_id": sess.device_id,
                "device_name": sess.device_name,
                "cli_version": env!("CARGO_PKG_VERSION"),
                "harnesses": harnesses,
            }),
        )
        .await
        .context("reportHarnesses failed")?;
    println!(
        "✓ reported harnesses on {} — available: {}",
        sess.device_name,
        if avail.is_empty() { "(none detected)".to_string() } else { avail.join(", ") }
    );
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

/// Hang local images on a message we authored — the general door for "the agent
/// made a picture, put it in the reply". Codex's own generated images are swept
/// up without this (see `harness::codex::ImageSweep`); every other harness, and
/// anything an agent draws with a script, comes through here.
async fn attach(client: &Client, files: &[String], message: Option<&str>) -> Result<()> {
    let msg = match message {
        Some(m) => m.to_string(),
        None => std::env::var("MAFOLD_DRAFT").ok().filter(|s| !s.is_empty()).context(
            "no message to attach to — run this inside an agent turn (the daemon sets \
             MAFOLD_DRAFT), or pass --message <id>",
        )?,
    };
    for f in files {
        let path = std::path::Path::new(f);
        client
            .attach_photo(&msg, path)
            .await
            .with_context(|| format!("attaching {}", path.display()))?;
        println!("✓ attached {}", path.display());
    }
    Ok(())
}

async fn send(client: &Client, chat: &str, channel: Option<&str>, text: &str) -> Result<()> {
    match channel {
        Some(ch) => {
            let (chat_id, ch) = channels::resolve(client, chat, ch).await?;
            let name = ch["name"].as_str().unwrap_or("?");
            client.send_to(Dest::chat(&chat_id).channel(ch["id"].as_str()), text).await?;
            println!("✓ sent to {chat} #{name}");
        }
        None => {
            let chat_id = client.resolve_chat(chat).await?;
            client.send_to(Dest::chat(&chat_id), text).await?;
            println!("✓ sent to {chat}");
        }
    }
    Ok(())
}
