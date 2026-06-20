//! Agent daemon — run Claude Code as your Mafold bot. Connects over WS, and for
//! each incoming message drives the local `claude` in the working directory and
//! streams the reply back. Always finalizes (never leaves the chat on "typing…").
//!
//! Context: each Mafold conversation maps to one persistent Claude Code session
//! (`--resume <session_id>`), so follow-ups keep the full prior context + tool
//! work. The map is persisted to ~/.mafold/sessions.json so context survives a
//! daemon restart (Claude Code stores the sessions on disk).

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify};

use crate::client::Client;
use crate::harness::{AgentEvent, Harness, Turn};

#[derive(Deserialize)]
struct Sender { username: String }
#[derive(Deserialize, Clone)]
struct InAttachment {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    url: Option<String>,
}
#[derive(Deserialize)]
struct IncomingMessage {
    conversation_id: String,
    sender: Sender,
    #[serde(default)]
    content: String,
    #[serde(default)]
    attachments: Vec<InAttachment>,
}

fn attachments_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".mafold").join("attachments")
}

// ── per-conversation Claude session map (persisted) ──
type Sessions = Arc<Mutex<HashMap<String, String>>>;

// ── per-conversation live control state (in-memory) ──
// `model` overrides the model for this chat (`/model …`); `cancel`, when a run
// is in flight, lets `/stop` interrupt it. Both keyed by conversation id.
#[derive(Default)]
struct ChatState {
    model: Option<String>,
    cancel: Option<Arc<Notify>>,
    /// When a `/login` is in flight in this chat, the channel that delivers the
    /// pasted auth code to the waiting `claude auth login` process.
    login_code_tx: Option<tokio::sync::mpsc::Sender<String>>,
}
type ChatStates = Arc<Mutex<HashMap<String, ChatState>>>;

/// The bot's OWNER-set config (from the server, via `getBot`), distilled to the
/// fields the daemon uses to drive harness defaults. Every field is OPTIONAL:
/// a missing/empty key keeps today's built-in behavior. Unknown keys are ignored.
#[derive(Default)]
struct OwnerConfig {
    /// Default model for turns when the chat hasn't overridden it (`/model`).
    model: Option<String>,
    /// Extra system prompt the owner set, appended to the mafold preamble.
    system_prompt: Option<String>,
    /// Default working directory when `--workdir` wasn't passed on the CLI.
    cwd: Option<String>,
}

impl OwnerConfig {
    /// Read the owner config via `getBot { username: <self> }` (callable by the
    /// bot itself). Best-effort: a failed call or absent keys yield an empty
    /// config, so the daemon falls back to its built-in defaults.
    async fn fetch(client: &Client, username: &str) -> Self {
        let detail = match client.bot(username).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("note: getBot failed ({e}) — using built-in defaults (no owner config)");
                return Self::default();
            }
        };
        // `config` is a flat `{key: value}` map of the owner's stored field
        // values (strings / JSON scalars). Read a string value for `key`,
        // trimming empties so a blank field is treated as unset.
        let get = |key: &str| -> Option<String> {
            detail["config"][key]
                .as_str()
                .map(str::to_string)
                // a numeric/bool scalar → render it as its JSON text
                .or_else(|| match &detail["config"][key] {
                    Value::Null => None,
                    other => Some(other.to_string()),
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        Self {
            model: get("model"),
            system_prompt: get("system_prompt"),
            // `cwd` is the documented key; accept `workdir` as an alias.
            cwd: get("cwd").or_else(|| get("workdir")),
        }
    }
}

fn sessions_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".mafold").join("sessions.json")
}
fn load_sessions() -> HashMap<String, String> {
    std::fs::read_to_string(sessions_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_sessions(map: &HashMap<String, String>) {
    if let Ok(s) = serde_json::to_string(map) {
        let _ = std::fs::write(sessions_path(), s);
    }
}

pub async fn run(client: Client, workdir: Option<String>, harness_id: String, auto_update: bool) -> Result<()> {
    // Self-update on startup (before connecting) so a (re)started agent is
    // always current; if it updates, re-exec into the new binary.
    if auto_update {
        if let Ok(Some(r)) = crate::update::check(&client.http).await {
            println!("↻ updating to v{}…", r.version);
            if crate::update::apply(&client.http, &r.url, &r.version, r.sha256.as_deref()).await.is_ok() {
                let _ = crate::update::reexec(); // replaces this process
            }
        }
    }

    let me = client.me().await.context("getMe failed — check the token / --base")?;
    let my_username = me["username"].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(!my_username.is_empty(), "could not resolve bot identity (bad token?)");

    // Cloud-first owner config: the bot reads its OWNER-set config from the server
    // and uses it to drive the harness defaults (model / system prompt / workdir).
    // Precedence everywhere is: explicit CLI flag > server owner-config > built-in
    // default (the same rule the `harness` selection already follows). Best-effort
    // — a missing/failed config just keeps today's behavior.
    let owner = OwnerConfig::fetch(&client, &my_username).await;

    // Cloud-first harness: the bot's server-configured harness wins over the
    // local `--harness` flag (which is the fallback / first-run default).
    let harness_id = me["harness"].as_str().filter(|s| !s.is_empty()).map(str::to_string).unwrap_or(harness_id);
    let harness = crate::harness::select(&harness_id);

    // Working dir: an explicit `--workdir` wins; else the owner-config `cwd`
    // (or `workdir`); else the current directory. Canonicalize so a relative
    // server value resolves the same way an explicit flag does.
    let workdir = workdir
        .or_else(|| owner.cwd.clone())
        .unwrap_or_else(|| ".".to_string());
    let workdir = std::fs::canonicalize(&workdir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(workdir);

    if !std::path::Path::new(&workdir).is_dir() {
        eprintln!("⚠️  working directory does not exist: {workdir} — the harness will fail. Check --workdir.");
    }
    if !harness.available() {
        eprintln!("⚠️  harness `{}` CLI not found on PATH — replies will fail until it's installed.", harness.id());
    }
    // Show the requested id and whether it fell back (an unimplemented harness
    // resolves to claude-code), so cloud-first selection is observable in logs.
    let harness_label = if harness.id() != harness_id {
        format!("{harness_id} (→ {} fallback)", harness.id())
    } else {
        harness_id.clone()
    };
    let model_label = owner.model.as_deref().unwrap_or("default");
    let sysprompt_label = if owner.system_prompt.is_some() { "  ·  +owner-system-prompt" } else { "" };
    println!("mafold agent ✓ connected as @{my_username}  ·  harness={harness_label}  ·  workdir={workdir}  ·  model={model_label}{sysprompt_label}");

    // Publish the command panel (the chat "/" menu): the daemon's own control
    // commands first, then every skill/slash-command the harness discovers on
    // this machine, so anyone chatting the bot can discover + tap them.
    publish_commands(&client, &workdir, &harness).await;

    let owner = Arc::new(owner);
    let sessions: Sessions = Arc::new(Mutex::new(load_sessions()));
    let chat_states: ChatStates = Arc::new(Mutex::new(HashMap::new()));
    // Serialize claude runs: same workdir → concurrent edits/sessions would
    // clash. One at a time (queued); each message still shows "typing" while it
    // waits because we open the draft before taking the lock.
    let exec_lock = Arc::new(Mutex::new(()));

    // Auto-update: poll every 10 minutes; if a newer release exists, apply +
    // re-exec — but only when IDLE (try_lock → no claude running/queued) so an
    // update never kills an in-flight reply. We ALSO check on every reconnect
    // (below), so a new release lands within minutes, not an hour.
    if auto_update {
        let client = client.clone();
        let exec_lock = exec_lock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                maybe_update(&client.http, &exec_lock).await;
            }
        });
    }

    // Reconnect loop: a dropped WS (network blip, server restart) must NOT kill
    // the daemon. Reconnect with backoff; sessions/exec_lock persist across it.
    let mut backoff = 1u64;
    let mut last_update_check = std::time::Instant::now();
    loop {
        if let Err(e) = connect_and_run(&client, &workdir, &my_username, &sessions, &exec_lock, &chat_states, &harness, &owner).await {
            eprintln!("connection error: {e}");
        }
        // A dropped WS is a natural idle moment → opportunistically self-update,
        // so a new release lands on reconnect (rate-limited to ≤ once / 5 min so
        // a reconnect storm doesn't hammer the releases API).
        if auto_update && last_update_check.elapsed() > Duration::from_secs(300) {
            last_update_check = std::time::Instant::now();
            maybe_update(&client.http, &exec_lock).await;
        }
        eprintln!("reconnecting in {backoff}s…");
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Check for a newer release; if one exists and the agent is IDLE (no claude run
/// in flight → `exec_lock` is free), safely apply it and re-exec into the new
/// binary. Idle-gated so a self-update never interrupts a reply; never returns
/// on a successful re-exec. Shared by the periodic poll + the reconnect check.
async fn maybe_update(http: &reqwest::Client, exec_lock: &Arc<Mutex<()>>) {
    match crate::update::check(http).await {
        Ok(Some(r)) => {
            // Hold the lock across apply + re-exec so no new run starts mid-update.
            if let Ok(_guard) = exec_lock.try_lock() {
                println!("↻ updating to v{} — restarting…", r.version);
                if crate::update::apply(http, &r.url, &r.version, r.sha256.as_deref()).await.is_ok() {
                    let _ = crate::update::reexec();
                }
            } else {
                println!("update v{} available — will apply when idle", r.version);
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("auto-update check failed: {e}"),
    }
}

/// The daemon's own control commands — handled locally, never forwarded to
/// `claude`. Listed first in the menu; see `handle_control`.
fn control_commands() -> Vec<Value> {
    vec![
        serde_json::json!({ "command": "clear",  "description": "Start a fresh conversation (clear context)" }),
        serde_json::json!({ "command": "new",    "description": "Alias for /clear" }),
        serde_json::json!({ "command": "stop",   "description": "Stop the reply that's currently running" }),
        serde_json::json!({ "command": "model",  "description": "Switch the model for this chat", "arg_hint": "name | reset" }),
        serde_json::json!({ "command": "status", "description": "Is the agent busy? show the working directory" }),
        serde_json::json!({ "command": "cwd",    "description": "Show the working directory" }),
        serde_json::json!({ "command": "help",   "description": "What this agent can do" }),
    ]
}

/// Build the full command panel (control commands + discovered skills/commands)
/// and publish it. Best-effort — a failure just leaves the previous menu.
async fn publish_commands(client: &Client, workdir: &str, harness: &Arc<dyn Harness>) {
    let mut commands = control_commands();
    if let Value::Array(discovered) = harness.discover(workdir) {
        commands.extend(discovered);
    }
    let n = commands.len();
    if client.set_commands(Value::Array(commands)).await.is_ok() {
        println!("published {n} commands (control + discovered skills) to the chat menu");
    }
}

/// One WS session: connect, keepalive-ping, dispatch incoming messages. Returns
/// when the socket drops (so the caller reconnects).
#[allow(clippy::too_many_arguments)]
async fn connect_and_run(
    client: &Client,
    workdir: &str,
    my_username: &str,
    sessions: &Sessions,
    exec_lock: &Arc<Mutex<()>>,
    chat_states: &ChatStates,
    harness: &Arc<dyn Harness>,
    owner: &Arc<OwnerConfig>,
) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(&client.ws_url())
        .await
        .context("WebSocket connect failed")?;
    let (mut write, mut read) = ws.split();
    println!("listening for messages to @{my_username} …");

    // Keepalive so the heartbeat keeps the bot marked online.
    let ping = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(25));
        loop {
            tick.tick().await;
            if write.send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into())).await.is_err() {
                break;
            }
        }
    });

    // Cards this bot can embed in replies — fetched once per connection, folded
    // into the mafold preamble each turn.
    let card_tags = available_card_tags(client).await;

    while let Some(frame) = read.next().await {
        let frame = match frame { Ok(f) => f, Err(e) => { eprintln!("ws error: {e}"); break; } };
        let text = match frame.into_text() { Ok(t) => t, Err(_) => continue };
        let env: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => continue };
        if env.get("method").and_then(|m| m.as_str()) != Some("events.messageNew") { continue; }
        let m: IncomingMessage = match serde_json::from_value(env["params"].clone()) { Ok(m) => m, Err(_) => continue };
        // Skip our own echoes + truly empty messages — but an image-only
        // message (empty text + attachments) is real, so keep it.
        if m.sender.username.eq_ignore_ascii_case(my_username)
            || (m.content.trim().is_empty() && m.attachments.is_empty()) { continue; }

        println!("← @{}: {}", m.sender.username, m.content);

        let trimmed = m.content.trim();

        // If a `/login` in this chat is waiting for the pasted Authentication
        // Code, this message IS that code — feed it to the login process (don't
        // treat it as a prompt). `/stop` cancels the sign-in.
        let pending_login = chat_states.lock().await.get(&m.conversation_id).and_then(|s| s.login_code_tx.clone());
        if let Some(tx) = pending_login {
            if trimmed.eq_ignore_ascii_case("/stop") || trimmed.eq_ignore_ascii_case("/cancel") {
                if let Some(s) = chat_states.lock().await.get_mut(&m.conversation_id) { s.login_code_tx = None; }
                let _ = client.send(&m.conversation_id, "Cancelled sign-in.").await;
            } else {
                let _ = tx.send(trimmed.to_string()).await;
                let _ = client.send(&m.conversation_id, "🔑 Got the code — finishing sign-in…").await;
            }
            continue;
        }

        // Daemon control commands (`/clear`, `/stop`, `/model`, …) are handled
        // locally and never reach claude. `/login` runs an interactive flow.
        // Any OTHER `/name …` falls through (emulated, mocked, or to claude).
        if let Some(rest) = trimmed.strip_prefix('/') {
            let mut it = rest.splitn(2, char::is_whitespace);
            let name = it.next().unwrap_or("").to_lowercase();
            let arg = it.next().unwrap_or("").trim();
            if name == "login" {
                let (client, chat_id, arg, chat_states) = (client.clone(), m.conversation_id.clone(), arg.to_string(), chat_states.clone());
                tokio::spawn(async move { login_flow(client, chat_id, arg, chat_states).await; });
                continue;
            }
            if is_control(&name) {
                handle_control(client, workdir, &m.conversation_id, &name, arg, sessions, chat_states, harness).await;
                continue;
            }
        }

        let client = client.clone();
        let workdir = workdir.to_string();
        let sessions = sessions.clone();
        let exec_lock = exec_lock.clone();
        let chat_states = chat_states.clone();
        let harness = harness.clone();
        let attachments = m.attachments.clone();
        let chat_id = m.conversation_id.clone();
        let content = m.content.clone();
        // Model precedence: per-chat `/model` override > owner-config default >
        // the harness default (None).
        let model = chat_states.lock().await.get(&chat_id).and_then(|s| s.model.clone())
            .or_else(|| owner.model.clone());
        // mafold awareness for this turn: identity + peer + embeddable cards,
        // plus the owner's extra system prompt (if any) appended after it.
        let mut sys = mafold_preamble(my_username, &m.sender.username, &card_tags);
        if let Some(extra) = &owner.system_prompt {
            sys.push_str("\n\n");
            sys.push_str(extra);
        }
        let system = Some(sys);
        tokio::spawn(async move {
            // Harness-emulated slash commands (config dumps, /logout, mocks);
            // anything not emulated falls through to the harness as a prompt.
            let trimmed = content.trim();
            if let Some(rest) = trimmed.strip_prefix('/') {
                let mut it = rest.splitn(2, char::is_whitespace);
                let name = it.next().unwrap_or("").to_lowercase();
                let arg = it.next().unwrap_or("").trim();
                match harness.command(&client, &chat_id, &name, arg, &workdir).await {
                    crate::harness::CommandOutcome::Reply(text) => { let _ = client.send(&chat_id, &text).await; return; }
                    crate::harness::CommandOutcome::Handled => return,
                    crate::harness::CommandOutcome::Forward => {}
                }
            }
            if let Err(e) = handle(&client, &workdir, &chat_id, &content, &attachments, &sessions, &exec_lock, &chat_states, &harness, model, system).await {
                eprintln!("handle error: {e}");
            }
        });
    }
    ping.abort();
    println!("disconnected.");
    Ok(())
}

/// Is this slash name one the daemon handles itself (vs a Claude Code skill)?
fn is_control(name: &str) -> bool {
    matches!(name, "clear" | "new" | "compact" | "stop" | "model" | "status" | "cwd" | "help")
}

/// Run a daemon control command. Replies in-chat; never invokes claude.
#[allow(clippy::too_many_arguments)]
async fn handle_control(
    client: &Client,
    workdir: &str,
    chat_id: &str,
    name: &str,
    arg: &str,
    sessions: &Sessions,
    chat_states: &ChatStates,
    harness: &Arc<dyn Harness>,
) {
    match name {
        "clear" | "new" => {
            {
                let mut s = sessions.lock().await;
                if s.remove(chat_id).is_some() { save_sessions(&s); }
            }
            let _ = client.send(chat_id, "🧹 Context cleared — starting fresh.").await;
        }
        "compact" => {
            // Genuinely compact this conversation's Claude session (summarize the
            // prior context to free tokens, keeping continuity). Spawned so the
            // (slow) claude run never blocks the message loop.
            let (client, workdir, chat_id, sessions) =
                (client.clone(), workdir.to_string(), chat_id.to_string(), sessions.clone());
            tokio::spawn(async move { compact_session(client, workdir, chat_id, sessions).await; });
        }
        "stop" => {
            let notify = chat_states.lock().await.get(chat_id).and_then(|s| s.cancel.clone());
            if let Some(n) = notify {
                n.notify_one();
                // The running task finalizes its draft with a stop notice.
            } else {
                let _ = client.send(chat_id, "Nothing is running right now.").await;
            }
        }
        "model" => {
            let mut states = chat_states.lock().await;
            let st = states.entry(chat_id.to_string()).or_default();
            if arg.is_empty() {
                let cur = st.model.clone().unwrap_or_else(|| "default".into());
                let _ = client.send(chat_id, &format!("Model for this chat: {cur}\nSet with `/model <name>` (e.g. opus, sonnet, haiku) or `/model reset`.")).await;
            } else if arg.eq_ignore_ascii_case("reset") || arg.eq_ignore_ascii_case("default") {
                st.model = None;
                let _ = client.send(chat_id, "Model reset to the agent default.").await;
            } else {
                st.model = Some(arg.to_string());
                let _ = client.send(chat_id, &format!("Model for this chat set to `{arg}`.")).await;
            }
        }
        "status" => {
            let busy = chat_states.lock().await.get(chat_id).map(|s| s.cancel.is_some()).unwrap_or(false);
            let model = chat_states.lock().await.get(chat_id).and_then(|s| s.model.clone()).unwrap_or_else(|| "default".into());
            let state = if busy { "running a reply now" } else { "idle" };
            let auth = harness.status_line().await;
            let auth_line = if auth.is_empty() { String::new() } else { format!("\n• {}: {auth}", harness.id()) };
            let _ = client.send(chat_id, &format!("Agent {state}.\n• harness: {}\n• workdir: {workdir}\n• model: {model}{auth_line}", harness.id())).await;
        }
        "cwd" => {
            let _ = client.send(chat_id, &format!("Working directory: {workdir}")).await;
        }
        "help" => {
            let _ = client.send(chat_id,
                "I'm a Claude Code agent running on this machine — message me a task and I keep context across the conversation.\n\nControl commands:\n• /clear (or /new) — start fresh\n• /compact — summarize the context to free up room (keeps continuity)\n• /stop — stop the running reply\n• /model <name> — switch model for this chat\n• /status · /cwd — agent info\n\nEverything else in the `/` menu is a Claude Code skill or command — tap one to run it.").await;
        }
        _ => {}
    }
}

/// `/compact` — run Claude Code's `/compact` on this conversation's resumed
/// session so the prior context is summarized (frees tokens, keeps continuity),
/// keep resuming the compacted session, and post a card. Best-effort: on any
/// failure it tells the user and leaves the existing session untouched.
async fn compact_session(client: Client, workdir: String, chat_id: String, sessions: Sessions) {
    let prior = sessions.lock().await.get(&chat_id).cloned();
    let Some(sid) = prior else {
        let _ = client
            .send(&chat_id, "Nothing to compact yet — send me a task first, then /compact to summarize the context.")
            .await;
        return;
    };
    let _ = client.send(&chat_id, "🗜️ Compacting the conversation…").await;
    let out = tokio::process::Command::new("claude")
        .arg("-p").arg("/compact")
        .arg("--resume").arg(&sid)
        .arg("--output-format").arg("json")
        .arg("--dangerously-skip-permissions")
        .current_dir(&workdir)
        .env_remove("CLAUDECODE")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_default();
            // Keep resuming the (now compacted) session for future turns.
            if let Some(new_sid) = v["session_id"].as_str() {
                let mut s = sessions.lock().await;
                s.insert(chat_id.clone(), new_sid.to_string());
                save_sessions(&s);
            }
            // Token counts → a progress-bar card. The compaction turn READS the
            // whole prior conversation (≈ context before) and WRITES the summary
            // that becomes the new context (≈ context after).
            let u = &v["usage"];
            let before = u["input_tokens"].as_u64().unwrap_or(0)
                + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let after = u["output_tokens"].as_u64().unwrap_or(0);
            if before == 0 {
                // e.g. "Not enough messages to compact." — nothing meaningful freed.
                let _ = client
                    .send(&chat_id, "Not much to compact yet — keep chatting, then /compact to summarize the context.")
                    .await;
            } else {
                let card = format!("{{% compact before=\"{before}\" after=\"{after}\" /%}}");
                let _ = client.send(&chat_id, &card).await;
            }
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let err = err.trim();
            let msg = if err.is_empty() {
                "Compaction failed — the context is unchanged.".to_string()
            } else {
                format!("Compaction failed — the context is unchanged.\n{err}")
            };
            let _ = client.send(&chat_id, &msg).await;
        }
        Err(e) => {
            let _ = client.send(&chat_id, &format!("Couldn't run compaction: {e}")).await;
        }
    }
}

/// Interactive `/login`: drive `claude auth login`, post the sign-in URL to the
/// chat (host browser suppressed), then write the Authentication Code the user
/// pastes back into the login process's stdin. Re-auths the agent's own `claude`.
async fn login_flow(client: Client, chat_id: String, arg: String, chat_states: ChatStates) {
    use tokio::io::AsyncWriteExt;
    let mode = if arg.contains("console") { "--console" } else { "--claudeai" };
    let _ = client.send(&chat_id, "🔐 Starting Anthropic sign-in… I'll post the link here; approve it, then paste the Authentication Code back to me. (This also re-authenticates the agent's own `claude`.)").await;

    // Suppress the host browser pop-up (macOS opens via `open <url>`): prepend a
    // no-op `open` to PATH + neutralize $BROWSER. COLUMNS keeps the URL unwrapped.
    let noop = noop_open_dir();
    let path = format!("{}:{}", noop.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = match tokio::process::Command::new("claude")
        .args(["auth", "login", mode])
        .env("PATH", path)
        .env("BROWSER", "/usr/bin/true")
        .env("COLUMNS", "4096")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => { let _ = client.send(&chat_id, &format!("Couldn't start `claude auth login`: {e}")).await; return; }
    };
    let mut stdin = child.stdin.take();

    // Merge stdout + stderr into one line channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(o) = child.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(async move { let mut l = BufReader::new(o).lines(); while let Ok(Some(line)) = l.next_line().await { let _ = tx.send(line); } });
    }
    if let Some(e) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move { let mut l = BufReader::new(e).lines(); while let Ok(Some(line)) = l.next_line().await { let _ = tx.send(line); } });
    }
    drop(tx);

    // Register the code channel so a pasted message reaches this flow.
    let (code_tx, mut code_rx) = tokio::sync::mpsc::channel::<String>(1);
    chat_states.lock().await.entry(chat_id.clone()).or_default().login_code_tx = Some(code_tx);

    // Phase 1: read output until we find + post the sign-in URL (60s budget).
    let post_url = async {
        while let Some(line) = rx.recv().await {
            if let Some(url) = extract_auth_url(&crate::commands::strip_ansi(&line)) {
                let _ = client.send(&chat_id, &format!("🔗 Open this to sign in (any device works):\n{url}\n\nAfter you approve, paste the Authentication Code here.")).await;
                return true;
            }
        }
        false
    };
    if !tokio::time::timeout(Duration::from_secs(60), post_url).await.unwrap_or(false) {
        let _ = child.start_kill();
        clear_login(&chat_states, &chat_id).await;
        let _ = client.send(&chat_id, "Couldn't get a sign-in link — this environment may need a TTY. Run `claude auth login` on the host.").await;
        return;
    }

    // Phase 2: wait for the pasted code (5 min), feed it to stdin, close stdin.
    match tokio::time::timeout(Duration::from_secs(300), code_rx.recv()).await {
        Ok(Some(code)) => {
            if let Some(mut si) = stdin.take() {
                let _ = si.write_all(format!("{}\n", code.trim()).as_bytes()).await;
                let _ = si.flush().await;
                // si drops here → stdin closes → claude proceeds with the code
            }
        }
        Ok(None) => { let _ = child.start_kill(); clear_login(&chat_states, &chat_id).await; return; } // cancelled
        Err(_) => {
            let _ = child.start_kill();
            clear_login(&chat_states, &chat_id).await;
            let _ = client.send(&chat_id, "Sign-in timed out — no code within 5 min. Try `/login` again.").await;
            return;
        }
    }

    // Phase 3: drain remaining output, await exit, report.
    while rx.recv().await.is_some() {}
    let ok = matches!(child.wait().await, Ok(st) if st.success());
    clear_login(&chat_states, &chat_id).await;
    if ok {
        let status = crate::commands::auth_status_line().await;
        let _ = client.send(&chat_id, &format!("✓ Signed in.{}", if status.is_empty() { String::new() } else { format!(" {status}") })).await;
    } else {
        let _ = client.send(&chat_id, "Sign-in didn't complete — the code may have been wrong or expired. Try `/login` again.").await;
    }
}

async fn clear_login(chat_states: &ChatStates, chat_id: &str) {
    if let Some(s) = chat_states.lock().await.get_mut(chat_id) { s.login_code_tx = None; }
}

/// Extract the OAuth sign-in URL from a line of `claude auth login` output —
/// only real `https://` links (NOT url-encoded `https%3A…` param values),
/// preferring the actual authorize endpoint so the relayed link is clickable.
fn extract_auth_url(line: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut from = 0usize;
    while let Some(rel) = line[from..].find("https://") {
        let start = from + rel;
        let tok: String = line[start..].chars().take_while(|c| !c.is_whitespace()).collect();
        let tok = tok.trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | '"' | '\'' | '>')).to_string();
        from = start + "https://".len();
        if tok.contains("authorize") || tok.contains("oauth") { return Some(tok); }
        if best.as_ref().map(|b| b.len() < tok.len()).unwrap_or(true) { best = Some(tok); }
    }
    best.filter(|u| u.len() > 24)
}

/// A directory holding a no-op `open` shim — stops the host browser from
/// launching during `/login` (the link is relayed to chat instead).
fn noop_open_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".mafold/noopbin");
    let _ = std::fs::create_dir_all(&dir);
    let open = dir.join("open");
    if !open.exists() && std::fs::write(&open, "#!/bin/sh\nexit 0\n").is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755));
    }
    dir
}

/// Card tags this bot can embed in replies (its family-scope + global published
/// cards). Best-effort — an empty list just omits the card menu.
async fn available_card_tags(client: &Client) -> Vec<String> {
    match client.list_cards().await {
        Ok(v) => v["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["tag"].as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The mafold-awareness system prompt appended each turn (`--append-system-prompt`):
/// who the bot is, the conversation it's replying in, and that it can embed cards
/// inline — so a "pure" coding agent knows it's acting as a Mafold bot.
fn mafold_preamble(bot: &str, peer: &str, cards: &[String]) -> String {
    let mut s = format!(
        "You are an AI agent running as a Mafold bot — your Mafold username is @{bot}. \
You are replying inside a Mafold conversation with @{peer}; your output this turn is \
delivered to that conversation as a chat message from you. Write a chat reply \
(conversational), not terminal output.\n\n\
You can embed CARDS in your reply: write a Markdoc tag inline and Mafold renders it as a \
native card. Write the tag directly — do NOT wrap it in a code fence or escape it:\n  \
{{% cardname attribute=\"value\" /%}}\n"
    );
    if cards.is_empty() {
        s.push_str("(No custom cards are published for you yet — plain text/Markdown is fine.)");
    } else {
        s.push_str("Cards available to embed here (each takes its own attributes):\n");
        for tag in cards {
            s.push_str(&format!("  • {{% {tag} … /%}}\n"));
        }
        s.push_str("Use a card when it communicates better than prose; otherwise reply normally.");
    }
    s
}

/// Open a draft, run claude (resuming this conversation's session), ALWAYS
/// finalize (surfacing any error).
#[allow(clippy::too_many_arguments)]
async fn handle(
    client: &Client,
    workdir: &str,
    chat_id: &str,
    prompt: &str,
    attachments: &[InAttachment],
    sessions: &Sessions,
    exec_lock: &Arc<Mutex<()>>,
    chat_states: &ChatStates,
    harness: &Arc<dyn Harness>,
    model: Option<String>,
    system: Option<String>,
) -> Result<()> {
    let msg_id = client.create_draft(chat_id).await?;

    // Download any image attachments locally so Claude can SEE them — its Read
    // tool renders images. We reference the saved paths in the prompt.
    let mut full_prompt = prompt.to_string();
    let mut saved: Vec<String> = vec![];
    for a in attachments {
        if a.kind != "photo" { continue; }
        let Some(url) = a.url.as_deref() else { continue };
        match client.download(url).await {
            Ok(bytes) => {
                let name = url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("image.jpg");
                let dir = attachments_dir();
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(name);
                if std::fs::write(&path, &bytes).is_ok() {
                    saved.push(path.to_string_lossy().into_owned());
                }
            }
            Err(e) => eprintln!("attachment download failed: {e}"),
        }
    }
    if !saved.is_empty() {
        let list = saved.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
        full_prompt = format!(
            "{prompt}\n\n[The user attached {} image(s). Use your Read tool to view them:\n{list}]",
            saved.len()
        );
    }

    // Serialize: only one turn runs at a time in this workdir.
    let _guard = exec_lock.lock().await;
    let prior = sessions.lock().await.get(chat_id).cloned();

    // Register a cancel handle so `/stop` can interrupt this run; cleared below.
    let cancel = Arc::new(Notify::new());
    chat_states.lock().await.entry(chat_id.to_string()).or_default().cancel = Some(cancel.clone());

    let turn = Turn {
        prompt: full_prompt,
        workdir: workdir.to_string(),
        session: prior.clone(),
        model,
        cancel: cancel.clone(),
        system,
    };

    // Renderer task: drain the harness's normalized events → batched, ordered
    // markdoc deltas (text + cards), so a slow append never stalls reading.
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let renderer = {
        let client = client.clone();
        let msg_id = msg_id.clone();
        tokio::spawn(render_loop(ev_rx, client, msg_id))
    };

    let result = harness.run(turn, ev_tx).await;
    let _ = renderer.await;

    match result {
        Ok(o) => {
            if o.stopped {
                let _ = client.append_delta(&msg_id, "\n\n⏹ Stopped.").await;
            } else if !o.produced {
                let _ = client.append_delta(&msg_id, "_(the agent produced no output)_").await;
            }
            // Persist the (possibly new) session so the next message resumes it.
            if let Some(sid) = o.session {
                let mut s = sessions.lock().await;
                if s.get(chat_id).map(String::as_str) != Some(sid.as_str()) {
                    s.insert(chat_id.to_string(), sid);
                    save_sessions(&s);
                }
            }
        }
        Err(e) => {
            eprintln!("harness run failed: {e}");
            // A stale/expired resumed session → drop it so the next message
            // starts fresh instead of failing again.
            if prior.is_some() {
                let mut s = sessions.lock().await;
                if s.remove(chat_id).is_some() { save_sessions(&s); }
            }
            let _ = client.append_delta(&msg_id, &format!("⚠️ Agent error: {e}")).await;
        }
    }
    // Clear the cancel handle (keep the per-chat model) now the run is over.
    if let Some(st) = chat_states.lock().await.get_mut(chat_id) { st.cancel = None; }
    let _ = client.finalize(&msg_id).await;
    println!("→ finalized reply for chat {chat_id}");
    Ok(())
}

/// Drains a harness's `AgentEvent` stream → batched, ordered markdoc deltas.
/// Batches small text chunks (≥240 bytes or ~120ms) so long replies stream
/// smoothly even on a slow link; cards render through the same path, in order.
async fn render_loop(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    client: Client,
    msg_id: String,
) {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut buf = String::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(120), rx.recv()).await {
            Ok(Some(ev)) => {
                if let Some(s) = crate::render::render(&ev, &mut names) {
                    buf.push_str(&s);
                    if buf.len() >= 240 {
                        let _ = client.append_delta(&msg_id, &buf).await;
                        buf.clear();
                    }
                }
            }
            Ok(None) => break, // harness done → channel closed
            Err(_) => {
                if !buf.is_empty() {
                    let _ = client.append_delta(&msg_id, &buf).await;
                    buf.clear();
                }
            }
        }
    }
    if !buf.is_empty() {
        let _ = client.append_delta(&msg_id, &buf).await;
    }
}
