//! Kimi Code harness — drives Moonshot's local `kimi` CLI headlessly
//! (`kimi --print --output-format stream-json`) and normalizes its message JSONL
//! into [`AgentEvent`]s. The renderer (`crate::render`) turns those into the same
//! chat text + cards every other harness produces, so card rendering is identical
//! — this file only has to speak Kimi's dialect.
//!
//! Differences from Claude Code / Codex that shape this impl:
//! - **OpenAI-style message stream, not a custom event envelope.** Kimi's print
//!   mode emits one `kosong` chat Message per line: `{role:"assistant", content,
//!   tool_calls?}` for the model and `{role:"tool", tool_call_id, content}` for a
//!   result. `content` is a bare string for a single text part, else an array of
//!   typed parts (`{type:"text",text}` / `{type:"think",think}`). Tool-call
//!   arguments are a JSON *string* (OpenAI function-call shape).
//! - **Session id lives on stderr, not the stream.** Kimi prints
//!   `To resume this session: kimi -r <UUID>` to stderr on exit; there is no
//!   session event on stdout, so we parse it from the drained stderr.
//! - **No usage in the stream.** Print mode never emits token/cost, so the
//!   end-of-turn `Done` carries none (no `{% result %}` card for Kimi turns).
//! - **No `--append-system-prompt`.** Like Codex, the mafold preamble is folded
//!   into the front of the prompt.
//! - **Thinking is a boolean, not a budget.** A `/think` budget (>0) maps to
//!   `--thinking`, 0 to `--no-thinking`; Kimi has no reasoning-effort tiers, so
//!   `effort` is ignored.
//! - **Auth lives on the host** (`kimi login`; the token is under
//!   `~/.kimi/credentials`); we don't strip it.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentEvent, CommandOutcome, Harness, Turn, TurnOutcome};
use crate::client::Client;

pub struct KimiCode;

#[async_trait]
impl Harness for KimiCode {
    fn id(&self) -> &'static str {
        "kimi-code"
    }

    fn available(&self) -> bool {
        super::on_path("kimi")
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        // `effort` (Kimi has no reasoning tiers) and `ask_file` (no AskUserQuestion
        // PreToolUse hook) don't apply — accepted and ignored.
        let Turn {
            prompt,
            workdir,
            session,
            model,
            effort: _,
            thinking,
            cancel,
            system,
            ask_file: _,
            conv,
        } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }

        // Kimi has no system-prompt flag; fold the mafold preamble into the prompt
        // so a mafold-unaware agent still knows it's acting as this bot.
        let full_prompt = match &system {
            Some(sys) if !sys.trim().is_empty() => format!("{sys}\n\n---\n\n{prompt}"),
            _ => prompt,
        };

        let p = RunParams {
            full_prompt: &full_prompt,
            workdir: &workdir,
            model: model.as_deref(),
            thinking,
            conv: &conv,
            cancel: &cancel,
            sink: &sink,
        };
        let out = run_once(&p, session.as_deref()).await?;
        // A stored session id can be foreign (the conversation carries a session
        // written by another harness) or dropped by Kimi. If the resume attempt
        // failed BEFORE streaming anything, and the reason looks session-related,
        // retry once fresh — the failed attempt emitted no event, so the retry
        // streams into a clean turn. (Narrow on purpose: never retry on an auth
        // or overload error, which a fresh run wouldn't fix and would only re-bill.)
        if session.is_some()
            && !out.produced
            && out.error.as_deref().is_some_and(looks_like_stale_session)
        {
            return run_once(&p, None).await;
        }
        Ok(out)
    }

    fn discover(&self, _workdir: &str) -> Value {
        // Kimi's custom slash commands are a TUI feature — a forwarded `/name`
        // would land as literal prompt text in headless `--print`, so publishing
        // them would only add dead menu entries. The daemon's own control commands
        // (/clear /new /model /status …) are added separately and DO work.
        Value::Array(vec![])
    }

    async fn command(&self, _client: &Client, _chat_id: &str, _name: &str, _arg: &str, _workdir: &str) -> CommandOutcome {
        // No emulated slash commands — anything that isn't a daemon control command
        // is forwarded to `kimi --print` as a prompt.
        CommandOutcome::Forward
    }

    async fn status_line(&self) -> String {
        if !super::on_path("kimi") {
            return String::new();
        }
        auth_status_line()
    }

    async fn cli_version(&self) -> String {
        kimi_version().await
    }
}

/// Borrowed per-turn invocation parameters shared by the resume attempt and its
/// fresh-session retry.
#[derive(Clone, Copy)]
struct RunParams<'a> {
    full_prompt: &'a str,
    workdir: &'a str,
    model: Option<&'a str>,
    thinking: Option<u32>,
    conv: &'a str,
    cancel: &'a std::sync::Arc<tokio::sync::Notify>,
    sink: &'a UnboundedSender<AgentEvent>,
}

/// One `kimi --print` invocation (optionally resuming `session`), streaming
/// normalized events into the sink.
async fn run_once(p: &RunParams<'_>, session: Option<&str>) -> Result<TurnOutcome> {
    let RunParams { full_prompt, workdir, model, thinking, conv, cancel, sink } = *p;

    let mut cmd = tokio::process::Command::new("kimi");
    // `--print` runs one turn non-interactively (and implies `--yolo`, so tools
    // run without approval prompts, which would hang a headless run).
    cmd.arg("--print").arg("--output-format").arg("stream-json");
    // Thinking: a mafold `/think` budget (>0) turns it on; 0 turns it off; unset
    // leaves Kimi's own config default (the k3 default is on).
    match thinking {
        Some(n) if n > 0 => {
            cmd.arg("--thinking");
        }
        Some(_) => {
            cmd.arg("--no-thinking");
        }
        None => {}
    }
    if let Some(m) = model {
        cmd.arg("--model").arg(map_model(m));
    }
    // Scope the agent to the bot's workdir (also the process cwd below). Strip a
    // Windows extended-length prefix (`\\?\C:\…`): the daemon canonicalizes the
    // workdir to that form, but Kimi's Python path handling wants a plain path.
    cmd.arg("--work-dir").arg(workdir.strip_prefix("\\\\?\\").unwrap_or(workdir));
    // Resume this conversation's Kimi session for context.
    if let Some(sid) = session {
        cmd.arg("--session").arg(sid);
    }
    // Export the current conversation for parity with the other harnesses (Kimi
    // has no room skill today — harmless).
    cmd.env("MAFOLD_CONV", conv);
    // Force Kimi's Python stdout to UTF-8. On Windows a piped (non-tty) Python
    // stdout defaults to the locale codepage (GBK on zh-CN), which emits invalid
    // UTF-8 for any non-ASCII content and corrupts the stream-json — the turn then
    // dies with "stream did not contain valid UTF-8". UTF-8 mode fixes it at the
    // source; the reader below also decodes lossily as a belt-and-suspenders.
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    // The prompt as a single `--prompt=…` arg so a value starting with `-` is
    // still taken literally (a bare `-p <value>` could mis-parse).
    cmd.arg(format!("--prompt={full_prompt}"));
    cmd.kill_on_drop(true);
    // Don't let the console child flash a window (the agent runs detached).
    crate::platform::no_window(&mut cmd);

    let mut child = cmd
        .current_dir(workdir)
        // NULL stdin: with a prompt arg, print mode won't read stdin — but a
        // piped, never-closing stdin could still make a tool (e.g. a shell that
        // reads input) block forever. /dev/null gives immediate EOF.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("couldn't run `kimi` in {workdir} — is Kimi Code installed and on PATH?"))?;
    // Register this run in the live-children set so a daemon shutdown kills exactly
    // THIS process (see harness::live_children); RAII deregisters on every path.
    let _child_guard = crate::harness::ChildGuard::new(child.id());

    let stdout = child.stdout.take().context("no stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();

    // Drain stderr CONCURRENTLY (same deadlock guard as the other harnesses) — and
    // Kimi prints its resumable session id there (`kimi -r <id>`), so we parse it
    // from the drained buffer once the run ends.
    let stderr_task = child.stderr.take().map(|se| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            // Read bytes + decode lossily (same reason as the stdout reader): Kimi
            // is Python-on-Windows and its stderr — where the resumable session id
            // is printed — can carry non-UTF-8 in the locale codepage. A strict
            // decode would fail and drop the session id with it, so every turn
            // would restart context-less. Lossy keeps the (ASCII) resume hint.
            let mut bytes = Vec::new();
            let mut se = se;
            let _ = se.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });

    let mut produced = false;
    let mut stopped = false;
    let mut session_id: Option<String> = None;
    let mut error: Option<String> = None;
    // Non-JSON stdout lines: Kimi prints a fatal error (bad auth, overload) as
    // plain text, not a stream-json line — captured to surface on a non-zero exit.
    let mut plain_text = String::new();

    // Stall watchdog — a healthy turn keeps stdout moving (message flushes, tool
    // results). Total silence past this is a hung run: kill it and surface the
    // reason, keeping the session so a resend resumes. Generous on purpose: Kimi
    // buffers a step's whole assistant message until its tool call resolves, and a
    // slow tool plus a 429 retry can be minutes of quiet.
    const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    loop {
        buf.clear();
        let n = tokio::select! {
            r = reader.read_until(b'\n', &mut buf) => r?,
            _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
            _ = tokio::time::sleep(STALL_AFTER) => {
                error = Some(format!(
                    "no output from Kimi for {} minutes — the run looks stalled and was stopped. Your context is kept; just resend to retry.",
                    STALL_AFTER.as_secs() / 60
                ));
                let _ = child.start_kill();
                break;
            }
        };
        if n == 0 {
            break; // EOF
        }
        // Decode lossily: Kimi is Python-on-Windows and can still emit a stray
        // non-UTF-8 byte; a hard `lines()` decode would kill the whole turn on it.
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => handle_message(&v, &sink, &mut produced),
            // A plain-text line (a fatal error Kimi printed outside the JSON
            // stream) — accumulate as the error text to surface on a bad exit.
            Err(_) => {
                if !plain_text.is_empty() {
                    plain_text.push('\n');
                }
                plain_text.push_str(line);
            }
        }
    }

    if stopped || error.is_some() {
        let _ = child.start_kill(); // idempotent
    }
    let status = child.wait().await; // reap
    let stderr_buf = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    if session_id.is_none() {
        session_id = parse_session_id(&stderr_buf);
    }

    if stopped {
        return Ok(TurnOutcome { produced, stopped: true, session: session_id, error });
    }
    if let Some(e) = error {
        return Ok(TurnOutcome { produced, stopped: false, session: session_id, error: Some(e) });
    }
    match status {
        Ok(s) if s.success() => {
            let _ = sink.send(AgentEvent::Done { duration_ms: None, cost_usd: None, tokens: None });
            Ok(TurnOutcome { produced, stopped: false, session: session_id, error: None })
        }
        Ok(s) => Ok(TurnOutcome {
            produced,
            stopped: false,
            session: session_id,
            error: Some(build_error(&plain_text, &stderr_buf, s.code())),
        }),
        Err(e) => bail!("waiting on `kimi` failed: {e}"),
    }
}

/// Normalize one Kimi message line into `AgentEvent`s. Assistant lines carry text
/// / thinking parts plus tool calls; `role:"tool"` lines carry a result; a line
/// with no `role` is a plan display or a system notification.
fn handle_message(v: &Value, sink: &UnboundedSender<AgentEvent>, produced: &mut bool) {
    match v["role"].as_str() {
        Some("assistant") => {
            emit_content(&v["content"], sink, produced);
            if let Some(calls) = v["tool_calls"].as_array() {
                for c in calls {
                    let id = c["id"].as_str().unwrap_or("").to_string();
                    let f = &c["function"];
                    let name = f["name"].as_str().unwrap_or("");
                    let args = f["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or_else(|| json!({}));
                    // The `Think` tool is the model journaling out loud — render it
                    // as a thinking block, not a tool card. (Its "recorded" result
                    // line is a non-bash tool_result the renderer drops anyway.)
                    if name == "Think" {
                        if let Some(t) = args["thought"].as_str() {
                            if !t.trim().is_empty() {
                                let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                                *produced = true;
                            }
                        }
                        continue;
                    }
                    let (canon, input) = map_tool(name, &args);
                    let _ = sink.send(AgentEvent::ToolCall { id, name: canon, input });
                    *produced = true;
                }
            }
        }
        Some("tool") => {
            let id = v["tool_call_id"].as_str().unwrap_or("").to_string();
            let text = strip_system_blocks(&content_text(&v["content"]));
            let _ = sink.send(AgentEvent::ToolResult { id, text });
            *produced = true;
        }
        // `user` / `system` echoes aren't emitted in print output — ignore.
        Some(_) => {}
        None => {
            if v["file_path"].is_string() && v["content"].is_string() {
                // PlanDisplay: the plan markdown, shown inline.
                if let Some(md) = v["content"].as_str() {
                    if !md.trim().is_empty() {
                        let _ = sink.send(AgentEvent::Text(format!("\n{md}\n")));
                        *produced = true;
                    }
                }
            } else if let Some(sev) = v["severity"].as_str() {
                // Notification — surface only warnings/errors, skip info noise.
                if matches!(sev, "error" | "warning") {
                    let title = v["title"].as_str().unwrap_or("");
                    let body = v["body"].as_str().unwrap_or("");
                    let line = format!("{title}: {body}");
                    let line = line.trim_matches(|c| c == ':' || c == ' ');
                    if !line.is_empty() {
                        let _ = sink.send(AgentEvent::Text(format!("\n> ⚠️ {line}\n")));
                        *produced = true;
                    }
                }
            }
        }
    }
}

/// Emit a message `content` (a bare string, or an ordered array of typed parts)
/// as `Text` / `Thinking` events, preserving order.
fn emit_content(content: &Value, sink: &UnboundedSender<AgentEvent>, produced: &mut bool) {
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                let _ = sink.send(AgentEvent::Text(s.clone()));
                *produced = true;
            }
        }
        Value::Array(parts) => {
            for p in parts {
                if p["type"].as_str() == Some("think") {
                    if let Some(t) = p["think"].as_str() {
                        if !t.trim().is_empty() {
                            let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                            *produced = true;
                        }
                    }
                } else if let Some(t) = p["text"].as_str() {
                    if !t.is_empty() {
                        let _ = sink.send(AgentEvent::Text(t.to_string()));
                        *produced = true;
                    }
                }
            }
        }
        _ => {}
    }
}

/// Map a Kimi tool call (`function.name` + parsed arguments) onto the renderer's
/// canonical tool name + input shape, so Kimi tools render as the same cards every
/// other harness gets (`bash` / `read` / `edit` / `todowrite` / …). Unknown tools
/// pass through as a generic `{% tool %}` card.
fn map_tool(name: &str, args: &Value) -> (String, Value) {
    match name {
        "Shell" => ("bash".into(), json!({ "command": args["command"].as_str().unwrap_or("") })),
        "ReadFile" | "ReadMediaFile" => ("read".into(), json!({ "file_path": args["path"].as_str().unwrap_or("") })),
        "WriteFile" => (
            "write".into(),
            json!({ "file_path": args["path"].as_str().unwrap_or(""), "content": args["content"].as_str().unwrap_or("") }),
        ),
        // Kimi's edit tool: `{path, edit: Edit | [Edit]}` where `Edit = {old, new,
        // replace_all}` → the renderer's edit / multiedit shape.
        "StrReplaceFile" => {
            let path = args["path"].as_str().unwrap_or("");
            match &args["edit"] {
                Value::Array(edits) => {
                    let mapped: Vec<Value> = edits
                        .iter()
                        .map(|e| json!({ "old_string": e["old"].as_str().unwrap_or(""), "new_string": e["new"].as_str().unwrap_or("") }))
                        .collect();
                    ("multiedit".into(), json!({ "file_path": path, "edits": mapped }))
                }
                e => (
                    "edit".into(),
                    json!({ "file_path": path, "old_string": e["old"].as_str().unwrap_or(""), "new_string": e["new"].as_str().unwrap_or("") }),
                ),
            }
        }
        "Glob" => ("glob".into(), json!({ "pattern": args["pattern"].as_str().unwrap_or("") })),
        "Grep" => ("grep".into(), json!({ "pattern": args["pattern"].as_str().unwrap_or("") })),
        "SearchWeb" => ("websearch".into(), json!({ "query": args["query"].as_str().unwrap_or("") })),
        "FetchURL" => ("webfetch".into(), json!({ "url": args["url"].as_str().unwrap_or("") })),
        // Kimi todos are `{title, status: pending|in_progress|done}` → the
        // renderer's `{content, status}` (its "done" is "completed").
        "SetTodoList" => {
            let todos: Vec<Value> = args["todos"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|t| {
                            json!({
                                "content": t["title"].as_str().unwrap_or(""),
                                "status": match t["status"].as_str().unwrap_or("pending") {
                                    "done" => "completed",
                                    "in_progress" => "in_progress",
                                    _ => "pending",
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            ("todowrite".into(), json!({ "todos": todos }))
        }
        "Agent" => (
            "task".into(),
            json!({
                "subagent_type": args["subagent_type"].as_str().unwrap_or("agent"),
                "description": args["description"].as_str().unwrap_or(""),
            }),
        ),
        // SendDMail, TaskList/TaskOutput/TaskStop, EnterPlanMode/ExitPlanMode, … →
        // a generic tool card (name + best-effort detail from the renderer).
        other => (other.to_string(), args.clone()),
    }
}

/// Text from a message `content` (a bare string, or an array of parts — the text
/// parts joined; think parts excluded).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"].as_str() != Some("think"))
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Drop `<system>…</system>` blocks (Kimi wraps a tool result's status message in
/// them before the raw output). For the `{% bash %}` card we want just the output;
/// text outside the blocks is kept verbatim.
fn strip_system_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<system>") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("</system>") {
            Some(end) => &rest[start + end + "</system>".len()..],
            None => &rest[start + "<system>".len()..],
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Kimi's stderr resume hint → the session id: `…kimi -r <UUID>`.
fn parse_session_id(stderr: &str) -> Option<String> {
    stderr
        .split("kimi -r ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// A resume failure that means "this session id is unusable" (foreign / dropped) —
/// safe to retry fresh. Narrow: never matches an auth or overload error.
fn looks_like_stale_session(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("session")
        && (m.contains("not found")
            || m.contains("no such")
            || m.contains("invalid")
            || m.contains("does not exist")
            || m.contains("unknown"))
}

/// Normalize a model value to a full Kimi model id. Owners — and the Kimi model's
/// own `{% customize %}` cards — often use a short name (`k3`, `k2.7`), but Kimi's
/// `--model` wants the full `kimi-code/…` id and errors on a bare short name.
/// Anything already namespaced (has a `/`) or unrecognized passes through
/// untouched, so `/model <anything-newer>` still reaches Kimi verbatim.
fn map_model(m: &str) -> String {
    let t = m.trim();
    if t.contains('/') {
        return t.to_string();
    }
    match t.to_lowercase().as_str() {
        "k3" => "kimi-code/k3".into(),
        "k2" | "k2.7" | "kimi-for-coding" | "coding" => "kimi-code/kimi-for-coding".into(),
        "highspeed" | "k2.7-highspeed" | "kimi-for-coding-highspeed" => {
            "kimi-code/kimi-for-coding-highspeed".into()
        }
        _ => t.to_string(),
    }
}

/// Build the surfaced error for a non-zero exit: the plain-text Kimi printed, else
/// a stderr line that isn't the resume hint, else a message from the exit code
/// (75 = `EX_TEMPFAIL`, Kimi's retryable class: overload / network).
fn build_error(plain_text: &str, stderr_buf: &str, code: Option<i32>) -> String {
    let p = plain_text.trim();
    if !p.is_empty() {
        return p.to_string();
    }
    let e = stderr_buf
        .lines()
        .filter(|l| !l.contains("To resume this session"))
        .collect::<Vec<_>>()
        .join("\n");
    let e = e.trim();
    if !e.is_empty() {
        return e.to_string();
    }
    match code {
        Some(75) => "Kimi hit a temporary error (overloaded or rate-limited). Your context is kept — just resend to retry.".into(),
        Some(c) => format!("kimi exited with status {c}"),
        None => "kimi exited abnormally".into(),
    }
}

/// `~/.kimi` (Kimi's home), honoring `KIMI_HOME` if set.
fn kimi_home() -> PathBuf {
    if let Some(h) = std::env::var_os("KIMI_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    PathBuf::from(base.unwrap_or_default()).join(".kimi")
}

/// One-line auth status for the `/status` Account row: whether the host has a
/// Kimi Code login, read from `~/.kimi/credentials/kimi-code.json` without
/// exposing the token. Empty handled by the caller (Kimi not installed).
fn auth_status_line() -> String {
    let path = kimi_home().join("credentials").join("kimi-code.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let logged_in = v["access_token"].as_str().is_some_and(|s| !s.is_empty());
            if logged_in {
                let scope = v["scope"].as_str().unwrap_or("kimi-code");
                format!("{scope} login")
            } else {
                "not logged in".into()
            }
        }
        Err(_) => "not logged in".into(),
    }
}

/// `kimi --version` → "1.49.0" (first numeric-ish token; "" if the CLI is missing).
async fn kimi_version() -> String {
    use std::time::Duration;
    let mut cmd = tokio::process::Command::new("kimi");
    cmd.arg("--version").stdin(Stdio::null());
    crate::platform::no_window(&mut cmd);
    match tokio::time::timeout(Duration::from_secs(8), cmd.output()).await {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Collect every AgentEvent a handler run emits (drain a sync channel).
    fn drain(f: impl FnOnce(&UnboundedSender<AgentEvent>, &mut bool)) -> (Vec<AgentEvent>, bool) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut produced = false;
        f(&tx, &mut produced);
        drop(tx);
        let mut out = vec![];
        let mut rx = rx;
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        (out, produced)
    }

    #[test]
    fn assistant_string_content_is_text() {
        let m = json!({ "role": "assistant", "content": "hi there" });
        let (evs, produced) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(produced);
        assert!(matches!(&evs[..], [AgentEvent::Text(t)] if t == "hi there"));
    }

    #[test]
    fn assistant_parts_split_think_and_text() {
        let m = json!({ "role": "assistant", "content": [
            { "type": "think", "think": "hmm", "encrypted": null },
            { "type": "text", "text": "answer" },
        ]});
        let (evs, _) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(matches!(&evs[0], AgentEvent::Thinking(t) if t == "hmm"));
        assert!(matches!(&evs[1], AgentEvent::Text(t) if t == "answer"));
    }

    #[test]
    fn shell_tool_call_becomes_bash() {
        let m = json!({ "role": "assistant", "content": "", "tool_calls": [
            { "type": "function", "id": "t1", "function": { "name": "Shell", "arguments": "{\"command\":\"ls -a\"}" } },
        ]});
        let (evs, produced) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(produced);
        match &evs[..] {
            [AgentEvent::ToolCall { id, name, input }] => {
                assert_eq!((id.as_str(), name.as_str()), ("t1", "bash"));
                assert_eq!(input["command"], "ls -a");
            }
            _ => panic!("expected one bash ToolCall, got {evs:?}"),
        }
    }

    #[test]
    fn read_and_write_remap_path_to_file_path() {
        let (name, input) = map_tool("ReadFile", &json!({ "path": "src/x.rs", "line_offset": 1 }));
        assert_eq!(name, "read");
        assert_eq!(input["file_path"], "src/x.rs");

        let (name, input) = map_tool("WriteFile", &json!({ "path": "a.txt", "content": "hello" }));
        assert_eq!(name, "write");
        assert_eq!(input["file_path"], "a.txt");
        assert_eq!(input["content"], "hello");
    }

    #[test]
    fn str_replace_single_and_list() {
        let (name, input) = map_tool("StrReplaceFile", &json!({ "path": "a.rs", "edit": { "old": "x", "new": "y" } }));
        assert_eq!(name, "edit");
        assert_eq!(input["file_path"], "a.rs");
        assert_eq!(input["old_string"], "x");
        assert_eq!(input["new_string"], "y");

        let (name, input) = map_tool(
            "StrReplaceFile",
            &json!({ "path": "b.rs", "edit": [ { "old": "a", "new": "b" }, { "old": "c", "new": "d" } ] }),
        );
        assert_eq!(name, "multiedit");
        let edits = input["edits"].as_array().unwrap();
        assert_eq!(edits[0]["old_string"], "a");
        assert_eq!(edits[1]["new_string"], "d");
    }

    #[test]
    fn set_todo_list_maps_title_and_done() {
        let (name, input) = map_tool(
            "SetTodoList",
            &json!({ "todos": [ { "title": "do a", "status": "done" }, { "title": "do b", "status": "in_progress" }, { "title": "do c", "status": "pending" } ] }),
        );
        assert_eq!(name, "todowrite");
        let todos = input["todos"].as_array().unwrap();
        assert_eq!(todos[0]["content"], "do a");
        assert_eq!(todos[0]["status"], "completed");
        assert_eq!(todos[1]["status"], "in_progress");
        assert_eq!(todos[2]["status"], "pending");
    }

    #[test]
    fn think_tool_becomes_thinking_not_a_card() {
        let m = json!({ "role": "assistant", "content": "", "tool_calls": [
            { "type": "function", "id": "t1", "function": { "name": "Think", "arguments": "{\"thought\":\"let me reason\"}" } },
        ]});
        let (evs, _) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(matches!(&evs[..], [AgentEvent::Thinking(t)] if t == "let me reason"));
    }

    #[test]
    fn tool_result_strips_system_wrapper() {
        let m = json!({ "role": "tool", "tool_call_id": "t1", "content": [
            { "type": "text", "text": "<system>Command executed successfully.</system>" },
            { "type": "text", "text": "file1\nfile2" },
        ]});
        let (evs, _) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(matches!(&evs[..], [AgentEvent::ToolResult { id, text }] if id == "t1" && text == "file1\nfile2"));
    }

    #[test]
    fn unknown_tool_passes_through() {
        let (name, input) = map_tool("SendDMail", &json!({ "to": "x", "body": "hi" }));
        assert_eq!(name, "SendDMail");
        assert_eq!(input["to"], "x");
    }

    #[test]
    fn parses_session_id_from_stderr() {
        let se = "\nTo resume this session: kimi -r 263d9dca-aafe-4628-9384-2f2174bc0233\n";
        assert_eq!(parse_session_id(se).as_deref(), Some("263d9dca-aafe-4628-9384-2f2174bc0233"));
        assert_eq!(parse_session_id("nothing here"), None);
    }

    #[test]
    fn stale_session_matcher_is_narrow() {
        assert!(looks_like_stale_session("Session not found: abc"));
        assert!(looks_like_stale_session("invalid session id"));
        // must NOT retry-fresh on auth / overload
        assert!(!looks_like_stale_session("401 the API key appears to be invalid or expired"));
        assert!(!looks_like_stale_session("Error code: 429 engine overloaded"));
    }

    #[test]
    fn model_shortnames_map_to_full_ids() {
        assert_eq!(map_model("k3"), "kimi-code/k3");
        assert_eq!(map_model("k2.7"), "kimi-code/kimi-for-coding");
        assert_eq!(map_model("K2.7"), "kimi-code/kimi-for-coding");
        assert_eq!(map_model("highspeed"), "kimi-code/kimi-for-coding-highspeed");
        // already a full id → untouched; unknown → passes through verbatim
        assert_eq!(map_model("kimi-code/k3"), "kimi-code/k3");
        assert_eq!(map_model("some-future-model"), "some-future-model");
    }

    #[test]
    fn plan_display_renders_as_text() {
        let m = json!({ "content": "# Plan\n1. do it", "file_path": "/tmp/plan.md" });
        let (evs, produced) = drain(|tx, p| handle_message(&m, tx, p));
        assert!(produced);
        assert!(matches!(&evs[..], [AgentEvent::Text(t)] if t.contains("# Plan")));
    }
}
