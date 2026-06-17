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

pub async fn run(client: Client, workdir: String, auto_update: bool) -> Result<()> {
    // Self-update on startup (before connecting) so a (re)started agent is
    // always current; if it updates, re-exec into the new binary.
    if auto_update {
        if let Ok(Some((v, url))) = crate::update::check(&client.http).await {
            println!("↻ updating to v{v}…");
            if crate::update::apply(&client.http, &url).await.is_ok() {
                let _ = crate::update::reexec(); // replaces this process
            }
        }
    }

    let me = client.me().await.context("getMe failed — check the token / --base")?;
    let my_username = me["username"].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(!my_username.is_empty(), "could not resolve bot identity (bad token?)");
    if !std::path::Path::new(&workdir).is_dir() {
        eprintln!("⚠️  working directory does not exist: {workdir} — claude will fail. Check --workdir.");
    }
    println!("mafold agent ✓ connected as @{my_username}  ·  workdir={workdir}");

    // Publish the command panel (the chat "/" menu): the daemon's own control
    // commands first, then every Claude Code skill/slash-command found on this
    // machine, so anyone chatting the bot can discover + tap them.
    publish_commands(&client, &workdir).await;

    let sessions: Sessions = Arc::new(Mutex::new(load_sessions()));
    let chat_states: ChatStates = Arc::new(Mutex::new(HashMap::new()));
    // Serialize claude runs: same workdir → concurrent edits/sessions would
    // clash. One at a time (queued); each message still shows "typing" while it
    // waits because we open the draft before taking the lock.
    let exec_lock = Arc::new(Mutex::new(()));

    // Hourly auto-update: check; if a newer release exists, apply + re-exec —
    // but only when IDLE (try_lock succeeds → no claude running/queued) so we
    // never kill an in-flight reply. Busy → retry next hour.
    if auto_update {
        let client = client.clone();
        let exec_lock = exec_lock.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                match crate::update::check(&client.http).await {
                    Ok(Some((v, url))) => {
                        if let Ok(_g) = exec_lock.try_lock() {
                            println!("↻ updating to v{v} — restarting…");
                            if crate::update::apply(&client.http, &url).await.is_ok() {
                                let _ = crate::update::reexec();
                            }
                        } else {
                            println!("update v{v} available — will apply when idle");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("auto-update check failed: {e}"),
                }
            }
        });
    }

    // Reconnect loop: a dropped WS (network blip, server restart) must NOT kill
    // the daemon. Reconnect with backoff; sessions/exec_lock persist across it.
    let mut backoff = 1u64;
    loop {
        if let Err(e) = connect_and_run(&client, &workdir, &my_username, &sessions, &exec_lock, &chat_states).await {
            eprintln!("connection error: {e}");
        }
        eprintln!("reconnecting in {backoff}s…");
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
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
async fn publish_commands(client: &Client, workdir: &str) {
    let mut commands = control_commands();
    if let Value::Array(discovered) = crate::discover::all(workdir) {
        commands.extend(discovered);
    }
    let n = commands.len();
    if client.set_commands(Value::Array(commands)).await.is_ok() {
        println!("published {n} commands (control + discovered skills) to the chat menu");
    }
}

/// One WS session: connect, keepalive-ping, dispatch incoming messages. Returns
/// when the socket drops (so the caller reconnects).
async fn connect_and_run(
    client: &Client,
    workdir: &str,
    my_username: &str,
    sessions: &Sessions,
    exec_lock: &Arc<Mutex<()>>,
    chat_states: &ChatStates,
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
                handle_control(client, workdir, &m.conversation_id, &name, arg, sessions, chat_states).await;
                continue;
            }
        }

        let client = client.clone();
        let workdir = workdir.to_string();
        let sessions = sessions.clone();
        let exec_lock = exec_lock.clone();
        let chat_states = chat_states.clone();
        let attachments = m.attachments.clone();
        let chat_id = m.conversation_id.clone();
        let content = m.content.clone();
        let model = chat_states.lock().await.get(&chat_id).and_then(|s| s.model.clone());
        tokio::spawn(async move {
            // Emulated Claude Code slash commands (config dumps, /login, /logout,
            // terminal-only mocks). Anything not emulated falls through to claude.
            let trimmed = content.trim();
            if let Some(rest) = trimmed.strip_prefix('/') {
                let mut it = rest.splitn(2, char::is_whitespace);
                let name = it.next().unwrap_or("").to_lowercase();
                let arg = it.next().unwrap_or("").trim();
                match crate::commands::handle(&name, arg, &workdir).await {
                    crate::commands::Outcome::Reply(text) => { let _ = client.send(&chat_id, &text).await; return; }
                    crate::commands::Outcome::Forward => {}
                }
            }
            if let Err(e) = handle(&client, &workdir, &chat_id, &content, &attachments, &sessions, &exec_lock, &chat_states, model).await {
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
    matches!(name, "clear" | "new" | "stop" | "model" | "status" | "cwd" | "help")
}

/// Run a daemon control command. Replies in-chat; never invokes claude.
async fn handle_control(
    client: &Client,
    workdir: &str,
    chat_id: &str,
    name: &str,
    arg: &str,
    sessions: &Sessions,
    chat_states: &ChatStates,
) {
    match name {
        "clear" | "new" => {
            {
                let mut s = sessions.lock().await;
                if s.remove(chat_id).is_some() { save_sessions(&s); }
            }
            let _ = client.send(chat_id, "🧹 Context cleared — starting fresh.").await;
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
            let auth = crate::commands::auth_status_line().await;
            let auth_line = if auth.is_empty() { String::new() } else { format!("\n• claude: {auth}") };
            let _ = client.send(chat_id, &format!("Agent {state}.\n• workdir: {workdir}\n• model: {model}{auth_line}")).await;
        }
        "cwd" => {
            let _ = client.send(chat_id, &format!("Working directory: {workdir}")).await;
        }
        "help" => {
            let _ = client.send(chat_id,
                "I'm a Claude Code agent running on this machine — message me a task and I keep context across the conversation.\n\nControl commands:\n• /clear (or /new) — start fresh\n• /stop — stop the running reply\n• /model <name> — switch model for this chat\n• /status · /cwd — agent info\n\nEverything else in the `/` menu is a Claude Code skill or command — tap one to run it.").await;
        }
        _ => {}
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
    model: Option<String>,
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

    // Serialize: only one claude runs at a time in this workdir.
    let _guard = exec_lock.lock().await;
    let prior = sessions.lock().await.get(chat_id).cloned();

    // Register a cancel handle so `/stop` can interrupt this run; cleared below.
    let cancel = Arc::new(Notify::new());
    chat_states.lock().await.entry(chat_id.to_string()).or_default().cancel = Some(cancel.clone());

    match stream_claude(client, workdir, &full_prompt, &msg_id, prior.as_deref(), sessions, chat_id, model.as_deref(), &cancel).await {
        Ok(o) if o.stopped => { let _ = client.append_delta(&msg_id, "\n\n⏹ Stopped.").await; }
        Ok(o) if o.produced => {}
        Ok(_) => { let _ = client.append_delta(&msg_id, "_(the agent produced no output)_").await; }
        Err(e) => {
            eprintln!("claude failed: {e}");
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

/// Outcome of one claude run.
struct RunOutcome {
    /// Any content (text or a card) was streamed.
    produced: bool,
    /// The run was interrupted by `/stop`.
    stopped: bool,
}

#[allow(clippy::too_many_arguments)]
async fn stream_claude(
    client: &Client,
    workdir: &str,
    prompt: &str,
    msg_id: &str,
    prior_session: Option<&str>,
    sessions: &Sessions,
    chat_id: &str,
    model: Option<&str>,
    cancel: &Arc<Notify>,
) -> Result<RunOutcome> {
    if !std::path::Path::new(workdir).is_dir() {
        anyhow::bail!("working directory does not exist: {workdir} — check --workdir");
    }
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p").arg(prompt)
        .arg("--output-format").arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--dangerously-skip-permissions");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    // Kill the child if its handle is dropped (e.g. the task is cancelled).
    cmd.kill_on_drop(true);
    // Resume this conversation's session → full prior context.
    if let Some(sid) = prior_session {
        cmd.arg("--resume").arg(sid);
    }
    let mut child = cmd
        .current_dir(workdir)
        .env_remove("CLAUDECODE")
        .env_remove("ANTHROPIC_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("couldn't run `claude` in {workdir} — is Claude Code installed and on PATH?"))?;

    let stdout = child.stdout.take().context("no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    // Decouple network sends from reading claude's stdout: a sender task drains
    // a channel and POSTs batched, ORDERED deltas. So a slow append never stalls
    // reading — long replies stream smoothly even on a slow link.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sender = {
        let client = client.clone();
        let msg_id = msg_id.to_string();
        tokio::spawn(async move {
            let mut buf = String::new();
            loop {
                match tokio::time::timeout(Duration::from_millis(120), rx.recv()).await {
                    Ok(Some(chunk)) => {
                        buf.push_str(&chunk);
                        if buf.len() >= 240 {
                            let _ = client.append_delta(&msg_id, &buf).await;
                            buf.clear();
                        }
                    }
                    Ok(None) => break,           // channel closed → claude done
                    Err(_) => {                  // idle ~120ms → flush what we have
                        if !buf.is_empty() {
                            let _ = client.append_delta(&msg_id, &buf).await;
                            buf.clear();
                        }
                    }
                }
            }
            if !buf.is_empty() { let _ = client.append_delta(&msg_id, &buf).await; }
        })
    };

    let mut produced = false;
    let mut stopped = false;
    let mut session_id: Option<String> = None;
    // tool_use_id → lowercased tool name, so a later tool_result (e.g. bash
    // output) can be matched back to the call that produced it.
    let mut tool_names: HashMap<String, String> = HashMap::new();

    loop {
        let line = tokio::select! {
            line = lines.next_line() => match line? { Some(l) => l, None => break },
            _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
        };
        let line = line.trim();
        if line.is_empty() { continue; }
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        if session_id.is_none() {
            if let Some(sid) = v["session_id"].as_str() {
                session_id = Some(sid.to_string());
            }
        }
        // Streaming assistant text.
        if v["type"] == "stream_event"
            && v["event"]["type"] == "content_block_delta"
            && v["event"]["delta"]["type"] == "text_delta"
        {
            if let Some(t) = v["event"]["delta"]["text"].as_str() {
                let _ = tx.send(t.to_string());
                produced = true;
            }
        }
        // Completed assistant message → emit cards for its tool calls + thinking
        // (text already streamed above, so it's skipped here). Shows the work as
        // it happens so a long agentic turn never looks frozen on "typing".
        if v["type"] == "assistant" {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("tool_use") => {
                            let _ = tx.send(tool_use_tag(b, &mut tool_names));
                            produced = true;
                        }
                        Some("thinking") => {
                            if let Some(tag) = thinking_tag(b) { let _ = tx.send(tag); produced = true; }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Tool results → output cards (currently: bash stdout/stderr).
        if v["type"] == "user" {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    if b["type"] == "tool_result" {
                        if let Some(tag) = tool_result_tag(b, &tool_names) { let _ = tx.send(tag); produced = true; }
                    }
                }
            }
        }
        if v["type"] == "result" {
            if let Some(tag) = result_tag(&v) { let _ = tx.send(tag); }
            break;
        }
    }
    drop(tx);             // close the channel → sender flushes the tail + exits
    let _ = sender.await;

    // Remember the session for this conversation so the next message resumes it.
    if let Some(sid) = session_id {
        let mut s = sessions.lock().await;
        if s.get(chat_id).map(String::as_str) != Some(sid.as_str()) {
            s.insert(chat_id.to_string(), sid);
            save_sessions(&s);
        }
    }

    if stopped {
        let _ = child.wait().await; // reap the killed child; don't treat as error
        return Ok(RunOutcome { produced, stopped });
    }
    let status = child.wait().await?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut se) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = se.read_to_string(&mut err).await;
        }
        let err = err.trim();
        anyhow::bail!("claude exited unsuccessfully{}", if err.is_empty() { String::new() } else { format!(": {err}") });
    }
    Ok(RunOutcome { produced, stopped })
}

/// A short, human-readable detail for a tool-use card (the file, command, …).
fn tool_detail(name: &str, input: &serde_json::Value) -> String {
    let raw = match name.to_lowercase().as_str() {
        "bash" => input["command"].as_str(),
        "edit" | "write" | "multiedit" | "read" | "notebookedit" => input["file_path"].as_str(),
        "glob" | "grep" => input["pattern"].as_str(),
        "webfetch" => input["url"].as_str(),
        "task" => input["description"].as_str(),
        "todowrite" => Some("updating plan"),
        _ => None,
    };
    raw.unwrap_or("").to_string()
}

/// Sanitize a value for a markdoc attribute: no quotes/newlines, capped length.
fn attr_esc(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c { '"' => '\'', '\n' | '\r' | '\t' => ' ', _ => c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > 80 {
        format!("{}…", cleaned.chars().take(80).collect::<String>())
    } else {
        cleaned.to_string()
    }
}

// ── rich card emission: map Claude Code stream-json → agent-card markdoc tags ──
// (The shared tag contract; rendered natively by mafold-web AgentCard + the iOS
// PrimitiveRenderer. Multi-line content goes in the tag body, not attributes.)

/// A `tool_use` block → the right card. Records `id → name` so a later
/// `tool_result` (bash output) can be matched back.
fn tool_use_tag(b: &Value, tool_names: &mut HashMap<String, String>) -> String {
    let name = b["name"].as_str().unwrap_or("tool");
    let lname = name.to_lowercase();
    if let Some(id) = b["id"].as_str() {
        tool_names.insert(id.to_string(), lname.clone());
    }
    let input = &b["input"];
    match lname.as_str() {
        "todowrite" => todo_tag(input),
        "edit" | "multiedit" => diff_tag_edit(&lname, input),
        "write" => diff_tag_write(input),
        "task" => format!(
            "\n{{% task subagent=\"{}\" desc=\"{}\" /%}}\n",
            attr_esc(input["subagent_type"].as_str().unwrap_or("agent")),
            attr_esc(input["description"].as_str().unwrap_or("")),
        ),
        "webfetch" => format!("\n{{% web url=\"{}\" /%}}\n", attr_esc(input["url"].as_str().unwrap_or(""))),
        "websearch" => format!("\n{{% web query=\"{}\" /%}}\n", attr_esc(input["query"].as_str().unwrap_or(""))),
        "skill" => {
            let sname = input["command"].as_str()
                .or_else(|| input["skill"].as_str())
                .or_else(|| input["name"].as_str())
                .unwrap_or("skill");
            let args = input["args"].as_str().or_else(|| input["arguments"].as_str()).unwrap_or("");
            format!("\n{{% skill name=\"{}\" args=\"{}\" /%}}\n", attr_esc(sname), attr_esc(args))
        }
        _ => {
            let detail = tool_detail(name, input);
            format!("\n{{% tool name=\"{}\" detail=\"{}\" /%}}\n", attr_esc(name), attr_esc(&detail))
        }
    }
}

/// `{% todo %}` from a TodoWrite call — one `[x]`/`[~]`/`[ ] item` line each.
fn todo_tag(input: &Value) -> String {
    let mut body = String::new();
    if let Some(items) = input["todos"].as_array() {
        for t in items {
            let content = t["content"].as_str().or_else(|| t["activeForm"].as_str()).unwrap_or("");
            let mark = match t["status"].as_str().unwrap_or("pending") {
                "completed" => 'x',
                "in_progress" => '~',
                _ => ' ',
            };
            body.push_str(&format!("[{mark}] {}\n", line_esc(content)));
        }
    }
    format!("\n{{% todo %}}\n{}{{% /todo %}}\n", block_esc(&body))
}

/// `{% diff %}` from an Edit/MultiEdit call.
fn diff_tag_edit(lname: &str, input: &Value) -> String {
    let file = input["file_path"].as_str().unwrap_or("");
    let (added, removed, body) = if lname == "multiedit" {
        let mut a = 0; let mut r = 0; let mut body = String::new();
        if let Some(edits) = input["edits"].as_array() {
            for e in edits {
                let (ea, er, eb) = synth_hunk(e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or(""));
                a += ea; r += er; body.push_str(&eb);
            }
        }
        (a, r, body)
    } else {
        synth_hunk(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or(""))
    };
    diff_tag(file, added, removed, &body)
}

/// `{% diff %}` from a Write call — the whole file as added lines.
fn diff_tag_write(input: &Value) -> String {
    let file = input["file_path"].as_str().unwrap_or("");
    let content = input["content"].as_str().unwrap_or("");
    let lines: Vec<&str> = if content.is_empty() { vec![] } else { content.lines().collect() };
    let mut body = String::new();
    for l in &lines { body.push('+'); body.push_str(l); body.push('\n'); }
    diff_tag(file, lines.len(), 0, &body)
}

fn diff_tag(file: &str, added: usize, removed: usize, body: &str) -> String {
    format!(
        "\n{{% diff file=\"{}\" added={} removed={} %}}\n{}{{% /diff %}}\n",
        attr_esc(file), added, removed, block_esc(&cap_lines(body, 24)),
    )
}

/// Naive line diff: all old lines as `-`, all new lines as `+`. Good enough for
/// the small, targeted edits Claude Code makes; the renderer colors +/- lines.
fn synth_hunk(old: &str, new: &str) -> (usize, usize, String) {
    let oldl: Vec<&str> = if old.is_empty() { vec![] } else { old.lines().collect() };
    let newl: Vec<&str> = if new.is_empty() { vec![] } else { new.lines().collect() };
    let mut body = String::new();
    for l in &oldl { body.push('-'); body.push_str(l); body.push('\n'); }
    for l in &newl { body.push('+'); body.push_str(l); body.push('\n'); }
    (newl.len(), oldl.len(), body)
}

/// `{% thinking %}` from an assistant thinking block (collapsed by renderers).
fn thinking_tag(b: &Value) -> Option<String> {
    let t = b["thinking"].as_str()?.trim();
    if t.is_empty() { return None; }
    Some(format!("\n{{% thinking %}}\n{}{{% /thinking %}}\n", block_esc(&cap_lines(t, 14))))
}

/// `{% bash %}` output card from a bash tool_result (matched via `tool_names`).
fn tool_result_tag(b: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
    let id = b["tool_use_id"].as_str()?;
    if tool_names.get(id).map(String::as_str) != Some("bash") { return None; }
    let out = tool_result_text(b);
    let out = out.trim();
    if out.is_empty() { return None; }
    Some(format!("\n{{% bash %}}\n{}{{% /bash %}}\n", block_esc(&cap_lines(out, 20))))
}

/// A tool_result's content can be a string or an array of `{type:text,text}`.
fn tool_result_text(b: &Value) -> String {
    match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `{% result %}` run-summary card from the final `type:"result"` event.
fn result_tag(v: &Value) -> Option<String> {
    let dur = v["duration_ms"].as_f64();
    let cost = v["total_cost_usd"].as_f64().filter(|c| *c > 0.0);
    let u = &v["usage"];
    let toks: u64 = ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .filter_map(|k| u[*k].as_u64())
        .sum();
    if dur.is_none() && cost.is_none() && toks == 0 { return None; }
    let mut attrs = String::new();
    if let Some(d) = dur { attrs.push_str(&format!(" duration=\"{:.1}s\"", d / 1000.0)); }
    if toks > 0 { attrs.push_str(&format!(" tokens=\"{}\"", fmt_count(toks))); }
    if let Some(c) = cost { attrs.push_str(&format!(" cost=\"${c:.4}\"")); }
    Some(format!("\n{{% result{attrs} /%}}\n"))
}

fn fmt_count(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

/// Keep the first `max` lines of a block, with a "+N more" marker; always ends
/// with a newline so the closing `{% /tag %}` sits on its own line.
fn cap_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        let mut out = lines.join("\n");
        if !out.is_empty() { out.push('\n'); }
        out
    } else {
        let shown = lines[..max].join("\n");
        format!("{shown}\n… (+{} more lines)\n", lines.len() - max)
    }
}

/// Neutralize stray markdoc markers inside a tag body so a value can't close the
/// tag early, and cap absolute length.
fn block_esc(s: &str) -> String {
    let mut out = s.replace("{%", "{ %").replace("%}", "% }");
    if out.chars().count() > 4000 {
        out = out.chars().take(4000).collect::<String>();
        out.push_str("\n…\n");
    }
    out
}

/// One-line, length-capped text for a todo item.
fn line_esc(s: &str) -> String {
    let one: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    let one = one.trim();
    if one.chars().count() > 120 {
        format!("{}…", one.chars().take(120).collect::<String>())
    } else {
        one.to_string()
    }
}
