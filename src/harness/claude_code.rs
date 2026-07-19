//! Claude Code harness — drives the local `claude` CLI headlessly
//! (`claude -p --output-format stream-json`) and normalizes its stream-json into
//! [`AgentEvent`]s. Skill/command discovery and the emulated slash commands live
//! in `crate::discover` / `crate::commands` (both Claude-Code-specific).

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentEvent, CommandOutcome, Harness, Turn, TurnOutcome};
use crate::client::Client;

pub struct ClaudeCode;

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn available(&self) -> bool {
        super::on_path("claude")
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let Turn { prompt, workdir, session, model, effort, thinking, cancel, system, ask_file, conv } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }
        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p").arg(&prompt)
            .arg("--output-format").arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--dangerously-skip-permissions")
            // Ignore the user's GLOBAL MCP servers (e.g. browser-use): claude
            // would otherwise spawn every configured MCP server on EVERY turn,
            // which pops a Python dock icon and adds seconds of startup latency
            // per reply. The daemon passes no --mcp-config, so this loads none.
            .arg("--strict-mcp-config");
        if let Some(m) = &model {
            cmd.arg("--model").arg(m);
        }
        // Reasoning effort (owner-set via Customization). No flag = Claude Code's
        // own default (currently xHigh).
        if let Some(e) = &effort {
            cmd.arg("--effort").arg(e);
        }
        // Export the current conversation so `mafold room …` (run by the agent
        // via the room skill) defaults to THIS room. Per-turn (not a global env)
        // because concurrent turns run different conversations.
        cmd.env("MAFOLD_CONV", &conv);
        // Extended thinking: a non-zero budget makes the model think before each
        // reply (streamed as `thinking` blocks). No env = Claude Code's default.
        if let Some(budget) = thinking {
            cmd.env("MAX_THINKING_TOKENS", budget.to_string());
        }
        // mafold awareness: who the bot is, the conversation, embeddable cards.
        if let Some(sys) = &system {
            cmd.arg("--append-system-prompt").arg(sys);
        }
        // Interactive AskUserQuestion: a PreToolUse hook intercepts the native
        // tool (which would otherwise auto-decline headless), blocks until the
        // user answers the chat card, then returns the answer as a deny-reason —
        // which claude feeds back as the tool result, same turn. The hook waits
        // on MAFOLD_ASK_FILE (the daemon writes the answer there). See ask_hook.
        if let Some(af) = &ask_file {
            cmd.env("MAFOLD_ASK_FILE", af);
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "mafold".into());
            let settings = serde_json::json!({
                "hooks": { "PreToolUse": [{
                    "matcher": "AskUserQuestion",
                    "hooks": [{ "type": "command", "command": format!("\"{exe}\" ask-hook") }]
                }, {
                    // Detach run_in_background Bash tasks into their own session
                    // (registered under ~/.mafold/bgtasks by MAFOLD_CONV) — claude
                    // kills its own background shells the moment it exits, so
                    // without this they can never outlive the turn. See bash_hook.
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": format!("\"{exe}\" bash-hook") }]
                }]}
            });
            cmd.arg("--settings").arg(settings.to_string());
        }
        cmd.kill_on_drop(true);
        if let Some(sid) = &session {
            cmd.arg("--resume").arg(sid);
        }
        // Don't let the console child flash a window (the agent runs detached).
        crate::platform::no_window(&mut cmd);
        let mut child = cmd
            .current_dir(&workdir)
            .env_remove("CLAUDECODE")
            .env_remove("ANTHROPIC_API_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("couldn't run `claude` in {workdir} — is Claude Code installed and on PATH?"))?;
        // Register this run in the live-children set so a daemon shutdown kills
        // exactly THIS process (see harness::live_children); RAII — deregisters
        // on every exit path.
        let _child_guard = crate::harness::ChildGuard::new(child.id());

        let stdout = child.stdout.take().context("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();

        // Drain stderr CONCURRENTLY: if `claude` writes more than the pipe buffer
        // (~64KB) to stderr while we're blocked reading stdout / on wait(), the
        // pipe fills, claude blocks on its write, and the turn deadlocks holding
        // the conversation lock. A reader task keeps stderr flowing the whole turn.
        let stderr_task = child.stderr.take().map(|se| {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let mut se = se;
                let _ = se.read_to_string(&mut buf).await;
                buf
            })
        });

        let mut produced = false;
        let mut stopped = false;
        let mut session_id: Option<String> = None;
        // Set when an API / execution error ends the turn — surfaced to the user.
        let mut error: Option<String> = None;
        // Real output-token progress for the generating heartbeat: the API's
        // `message_delta` usage is cumulative PER assistant message, so completed
        // messages accumulate into `tokens_done` when the next one starts.
        let mut tokens_done: u64 = 0;
        let mut tokens_cur: u64 = 0;

        // Stall watchdog: a healthy turn always keeps stdout moving (text deltas,
        // tool events, thinking) — even a long tool call is bracketed by its
        // tool_use/tool_result events within the tool's own timeout. A child that
        // goes fully silent longer than this is hung (the "typing forever, never
        // sends" failure): kill it and surface the reason through the error path,
        // which keeps the session so a resend resumes with context. Generous on
        // purpose — the longest legitimate silence is a slow tool run.
        const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line? { Some(l) => l, None => break },
                _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
                _ = tokio::time::sleep(STALL_AFTER) => {
                    error = Some(format!(
                        "no output from the agent for {} minutes — the run looks stalled and was stopped. Your context is kept; just resend to retry.",
                        STALL_AFTER.as_secs() / 60
                    ));
                    let _ = child.start_kill();
                    break;
                }
            };
            let line = line.trim();
            if line.is_empty() { continue; }
            let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
            if session_id.is_none() {
                if let Some(sid) = v["session_id"].as_str() { session_id = Some(sid.to_string()); }
            }
            // A fatal error event (NOT a transient API blip the SDK retries away
            // silently) — stop now and report the reason, instead of relaying an
            // endless error/retry stream while the turn never finalizes.
            if v["type"] == "error" {
                error = Some(
                    v["error"].as_str()
                        .or_else(|| v["message"].as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string()),
                );
                let _ = child.start_kill();
                break;
            }
            // Streaming assistant text — plus silent progress pulses for the
            // deltas that are NOT rendered (thinking / tool-arg json): they keep
            // the generating card's heartbeat honest while the model works
            // without visible output.
            if v["type"] == "stream_event" {
                let ev_type = &v["event"]["type"];
                if ev_type == "content_block_delta" {
                    let d = &v["event"]["delta"];
                    if d["type"] == "text_delta" {
                        if let Some(t) = d["text"].as_str() {
                            let _ = sink.send(AgentEvent::Text(t.to_string()));
                            produced = true;
                        }
                    } else if let Some(t) = d["thinking"].as_str().or_else(|| d["partial_json"].as_str()) {
                        let _ = sink.send(AgentEvent::Pulse { chars: t.len() as u64, tokens: None });
                    }
                } else if ev_type == "message_start" {
                    // A new assistant message: bank the previous one's usage.
                    tokens_done += std::mem::take(&mut tokens_cur);
                } else if ev_type == "message_delta" {
                    // Cumulative REAL output tokens for the current message.
                    if let Some(t) = v["event"]["usage"]["output_tokens"].as_u64() {
                        tokens_cur = t;
                        let _ = sink.send(AgentEvent::Pulse { chars: 0, tokens: Some(tokens_done + tokens_cur) });
                    }
                }
            }
            // Completed assistant message → tool calls + thinking (text already streamed).
            if v["type"] == "assistant" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        match b["type"].as_str() {
                            Some("tool_use") => {
                                let _ = sink.send(AgentEvent::ToolCall {
                                    id: b["id"].as_str().unwrap_or("").to_string(),
                                    name: b["name"].as_str().unwrap_or("tool").to_string(),
                                    input: b["input"].clone(),
                                });
                                produced = true;
                            }
                            Some("thinking") => {
                                if let Some(t) = b["thinking"].as_str() {
                                    if !t.trim().is_empty() {
                                        let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                                        produced = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Tool results (e.g. bash output).
            if v["type"] == "user" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "tool_result" {
                            if let Some(id) = b["tool_use_id"].as_str() {
                                let _ = sink.send(AgentEvent::ToolResult { id: id.to_string(), text: tool_result_text(b) });
                                produced = true;
                            }
                        }
                    }
                }
            }
            if v["type"] == "result" {
                // A non-success result means the agent GAVE UP (API error, max
                // turns, exec error). Surface the specific reason and stop
                // cleanly, instead of ending as if it had succeeded (previously
                // any `result` was reported as a normal Done — errors vanished).
                let subtype = v["subtype"].as_str().unwrap_or("");
                if v["is_error"].as_bool().unwrap_or(false) || (!subtype.is_empty() && subtype != "success") {
                    error = Some(
                        v["result"].as_str().map(str::trim).filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("agent ended with `{}`", if subtype.is_empty() { "error" } else { subtype })),
                    );
                    break;
                }
                let u = &v["usage"];
                let toks: u64 = ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                    .iter().filter_map(|k| u[*k].as_u64()).sum();
                let _ = sink.send(AgentEvent::Done {
                    duration_ms: v["duration_ms"].as_f64(),
                    cost_usd: v["total_cost_usd"].as_f64(),
                    tokens: if toks > 0 { Some(toks) } else { None },
                });
                break;
            }
        }
        drop(sink); // close → the renderer flushes its tail

        if stopped || error.is_some() {
            let _ = child.start_kill(); // idempotent; ensures the error path stops it too
            let _ = child.wait().await; // reap; the exit status is irrelevant here
            if let Some(t) = stderr_task { t.abort(); }
            return Ok(TurnOutcome { produced, stopped, session: session_id, error });
        }
        let status = child.wait().await?;
        if !status.success() {
            // The concurrent reader already drained stderr (no post-wait read that
            // could have deadlocked) — just collect what it captured.
            let err = match stderr_task {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            let err = err.trim();
            bail!("claude exited unsuccessfully{}", if err.is_empty() { String::new() } else { format!(": {err}") });
        }
        if let Some(t) = stderr_task { t.abort(); }
        Ok(TurnOutcome { produced, stopped, session: session_id, error: None })
    }

    fn discover(&self, workdir: &str) -> Value {
        crate::discover::all(workdir)
    }

    async fn command(&self, _client: &Client, _chat_id: &str, name: &str, arg: &str, workdir: &str) -> CommandOutcome {
        match crate::commands::handle(name, arg, workdir).await {
            crate::commands::Outcome::Reply(text) => CommandOutcome::Reply(text),
            crate::commands::Outcome::Forward => CommandOutcome::Forward,
        }
    }

    async fn status_line(&self) -> String {
        crate::commands::auth_status_line().await
    }

    async fn cli_version(&self) -> String {
        crate::commands::claude_version().await
    }
}

/// A tool_result's `content` can be a string or an array of `{type:text,text}`.
fn tool_result_text(b: &Value) -> String {
    match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
