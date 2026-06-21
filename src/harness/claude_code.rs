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
        on_path("claude")
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let Turn { prompt, workdir, session, model, cancel, system, ask_file } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }
        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p").arg(&prompt)
            .arg("--output-format").arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--dangerously-skip-permissions");
        if let Some(m) = &model {
            cmd.arg("--model").arg(m);
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
                }]}
            });
            cmd.arg("--settings").arg(settings.to_string());
        }
        cmd.kill_on_drop(true);
        if let Some(sid) = &session {
            cmd.arg("--resume").arg(sid);
        }
        let mut child = cmd
            .current_dir(&workdir)
            .env_remove("CLAUDECODE")
            .env_remove("ANTHROPIC_API_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("couldn't run `claude` in {workdir} — is Claude Code installed and on PATH?"))?;

        let stdout = child.stdout.take().context("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();

        let mut produced = false;
        let mut stopped = false;
        let mut session_id: Option<String> = None;

        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line? { Some(l) => l, None => break },
                _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
            };
            let line = line.trim();
            if line.is_empty() { continue; }
            let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
            if session_id.is_none() {
                if let Some(sid) = v["session_id"].as_str() { session_id = Some(sid.to_string()); }
            }
            // Streaming assistant text.
            if v["type"] == "stream_event"
                && v["event"]["type"] == "content_block_delta"
                && v["event"]["delta"]["type"] == "text_delta"
            {
                if let Some(t) = v["event"]["delta"]["text"].as_str() {
                    let _ = sink.send(AgentEvent::Text(t.to_string()));
                    produced = true;
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

        if stopped {
            let _ = child.wait().await; // reap the killed child; not an error
            return Ok(TurnOutcome { produced, stopped, session: session_id });
        }
        let status = child.wait().await?;
        if !status.success() {
            let mut err = String::new();
            if let Some(mut se) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = se.read_to_string(&mut err).await;
            }
            let err = err.trim();
            bail!("claude exited unsuccessfully{}", if err.is_empty() { String::new() } else { format!(": {err}") });
        }
        Ok(TurnOutcome { produced, stopped, session: session_id })
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
}

/// A tool_result's `content` can be a string or an array of `{type:text,text}`.
fn tool_result_text(b: &Value) -> String {
    match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Is `bin` resolvable on `$PATH`? (cheap availability check, no spawn)
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let p = dir.join(bin);
            p.is_file() || std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false)
        })
    })
}
