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

    /// Yes — via the PostToolUse hook registered in [`Self::run`].
    fn can_steer(&self) -> bool {
        true
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let Turn { prompt, workdir, session, model, effort, thinking, cancel, system, ask_file, steer_file, conv, surface, draft } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }
        let mut cmd = tokio::process::Command::new(super::program("claude"));
        // `-p` with NO prompt argument: the prompt goes in on stdin instead (see
        // the write below). It is the one input here that grows without bound —
        // it carries the conversation — and Windows hard-caps a command line at
        // 32,767 UTF-16 units, so on argv a long enough chat makes `CreateProcessW`
        // refuse the spawn outright (os error 206, ERROR_FILENAME_EXCED_RANGE).
        // Unconditionally, not past some Windows-only threshold: a size cliff that
        // only one platform falls off, and only on long conversations, is exactly
        // the kind of special case that gets shipped untested. `run_claude_stdin`
        // feeds /usage the same way.
        cmd.arg("-p")
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
        // The surface (conv + forum channel) the reply lands on — the bash-hook
        // registers detached background tasks under it, so their wrap-up turn
        // comes back to THIS channel instead of leaking into another one.
        cmd.env("MAFOLD_SURFACE", &surface);
        // The reply being streamed right now — `mafold attach <file>` hangs
        // media on it, so an image the agent makes lands in the same bubble as
        // the text about it.
        cmd.env("MAFOLD_DRAFT", &draft);
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
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| "mafold".into());
        let mut pre: Vec<serde_json::Value> = Vec::new();
        let mut post: Vec<serde_json::Value> = Vec::new();
        if let Some(af) = &ask_file {
            cmd.env("MAFOLD_ASK_FILE", af);
            pre.push(serde_json::json!({
                "matcher": "AskUserQuestion",
                "hooks": [{ "type": "command", "command": format!("\"{exe}\" ask-hook") }]
            }));
            pre.push(serde_json::json!({
                // Detach run_in_background Bash tasks into their own session
                // (registered under ~/.mafold/bgtasks by MAFOLD_CONV) — claude
                // kills its own background shells the moment it exits, so
                // without this they can never outlive the turn. See bash_hook.
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": format!("\"{exe}\" bash-hook") }]
            }));
        }
        // Mid-turn steering: what the user says while this turn runs reaches the
        // model at the next tool-result boundary. PostToolUse, matching every
        // tool, so the tool that was running when they spoke finishes normally
        // and nothing already on screen is un-said. See `steer_hook`.
        if let Some(sf) = &steer_file {
            cmd.env("MAFOLD_STEER_FILE", sf);
            post.push(serde_json::json!({
                "matcher": "*",
                "hooks": [{ "type": "command", "command": format!("\"{exe}\" steer-hook") }]
            }));
        }
        if !pre.is_empty() || !post.is_empty() {
            let mut hooks = serde_json::Map::new();
            if !pre.is_empty() {
                hooks.insert("PreToolUse".into(), serde_json::Value::Array(pre));
            }
            if !post.is_empty() {
                hooks.insert("PostToolUse".into(), serde_json::Value::Array(post));
            }
            let settings = serde_json::json!({ "hooks": hooks });
            cmd.arg("--settings").arg(settings.to_string());
        }
        cmd.kill_on_drop(true);
        if let Some(sid) = &session {
            cmd.arg("--resume").arg(sid);
            // Somebody else is holding this exact transcript right now (a VS
            // Code tab, a terminal). Print-mode `--resume` does NOT fork — it
            // hands back the same session id and appends to the same file — so
            // without this both writers braid into one session tree and the
            // resume pointer ends up wherever the last write landed. Forking
            // inherits everything they've typed up to this instant and leaves
            // their thread alone, which is what `/resume` has been promising in
            // words all along. The new id arrives on the stream (`session_id`)
            // and is what the caller stores, so this costs one fork, not one
            // per turn.
            if crate::commands::session_held_elsewhere(sid) {
                cmd.arg("--fork-session");
            }
        }
        // Don't let the console child flash a window (the agent runs detached).
        crate::platform::no_window(&mut cmd);
        let mut child = cmd
            .current_dir(&workdir)
            .env_remove("CLAUDECODE")
            .env_remove("ANTHROPIC_API_KEY")
            // Piped, never inherited: this carries the prompt, and inheriting the
            // daemon's stdin also made the spawn depend on whatever handle it
            // happens to hold (a duplication Windows can refuse on its own).
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| super::spawn_err("claude", &workdir, e))?;
        // Register this run in the live-children set so a daemon shutdown kills
        // exactly THIS process (see harness::live_children); RAII — deregisters
        // on every exit path.
        let _child_guard = crate::harness::ChildGuard::new(child.id());

        // Feed the prompt in its OWN task: a prompt past the pipe buffer (~64KB —
        // and this one holds the conversation) would otherwise block us here while
        // claude is blocked writing stdout that nobody is reading yet. Dropping
        // the handle closes stdin, which is the EOF `-p` waits for.
        if let Some(mut si) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = si.write_all(prompt.as_bytes()).await;
                let _ = si.shutdown().await;
            });
        }

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
        // Did the CURRENT assistant message arrive as text deltas? Reset on every
        // `message_start`. Reply text has three possible carriers (partial deltas,
        // the completed `assistant` message, the final `result`) and claude does
        // not guarantee the first one — when the partial stream is missing, the
        // completed message is the only copy we get. Streaming stays the primary
        // path; this flag is what keeps the fallbacks below from double-posting it.
        let mut streamed_text = false;
        // The text of the assistant message CURRENTLY streaming, cleared the
        // moment that message lands as a completed `assistant` event. A
        // `message_start` arriving while this is non-empty means the message it
        // belongs to never landed: the API connection dropped mid-response and
        // claude is retrying it by re-streaming from the first token, so the
        // partial attempt has to be un-said before the retry re-says it.
        let mut msg_text = String::new();
        let mut session_id: Option<String> = None;
        // Set when an API / execution error ends the turn — surfaced to the user.
        let mut error: Option<String> = None;
        // The last few NON-JSON stdout lines. `claude` prints its fatal reasons
        // as plain text on stdout — a usage cap, an auth failure, a `--resume`
        // id whose transcript is gone — NOT as stream-json, and the parser below
        // drops every line it can't parse. When the run then exits nonzero with
        // an empty stderr, this tail is the ONLY explanation that exists; without
        // it the daemon reported a bare "claude exited unsuccessfully" and the
        // reason was destroyed at the exact moment it was needed.
        let mut plain_tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        // Real output-token progress for the generating heartbeat: the API's
        // `message_delta` usage is cumulative PER assistant message, so completed
        // messages accumulate into `tokens_done` when the next one starts.
        let mut tokens_done: u64 = 0;
        let mut tokens_cur: u64 = 0;
        // Receipts for OTHER queued messages stepped over so far (see the
        // `result` arm). Bounded so a pathological queue can't hold a turn open
        // forever — past the bound we take the next receipt as ours and end the
        // turn, which is the old behaviour, not a new way to hang.
        const MAX_SKIPPED_RECEIPTS: u32 = 8;
        let mut skipped_receipts: u32 = 0;

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
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                // Not stream-json → keep it as a breadcrumb (see `plain_tail`)
                // instead of dropping it on the floor.
                Err(_) => { push_plain(&mut plain_tail, line); continue }
            };
            if session_id.is_none() {
                if let Some(sid) = v["session_id"].as_str() { session_id = Some(sid.to_string()); }
            }
            // Claude Code compacting its OWN context, mid-turn. It runs for
            // minutes (135s / 162s / 308s in this machine's transcripts) and
            // streams nothing at all while it works, so an unrelayed compaction
            // reads as a hung reply — exactly the moment a user gives up and
            // resends. Compaction is in place: the session id doesn't change,
            // so `--resume` is unaffected and nothing needs re-persisting.
            //
            // Deliberately does NOT set `produced`: this is our narration, not
            // model output. A turn that only compacted and then said nothing is
            // still an empty turn, and must stay eligible for the caller's
            // empty-turn retry.
            if v["type"] == "system" && v["subtype"] == "compact_boundary" {
                let _ = sink.send(AgentEvent::Compacted { pre_tokens: compaction_pre_tokens(&v) });
                continue;
            }
            // Usage-limit state. Relayed ONLY when it is not "allowed": claude
            // emits one of these on ordinary healthy turns too, and echoing
            // "your quota is fine" into every reply is noise, not news.
            if v["type"] == "rate_limit_event" {
                if let Some((kind, resets_at)) = rate_limit_notice(&v["rate_limit_info"]) {
                    let _ = sink.send(AgentEvent::RateLimited { kind, resets_at });
                }
                continue;
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
                            msg_text.push_str(t);
                            produced = true;
                            streamed_text = true;
                        }
                    } else if let Some(t) = d["thinking"].as_str().or_else(|| d["partial_json"].as_str()) {
                        let _ = sink.send(AgentEvent::Pulse { chars: t.len() as u64, tokens: None });
                    }
                } else if ev_type == "message_start" {
                    // Text still pending from the LAST message_start means that
                    // message never completed — this is a retry of it, not the
                    // next message, and it will re-stream from the first token.
                    // Take back the abandoned attempt (the transcript removes it
                    // only if it is still the exact tail, so a producer that
                    // resumed instead of restarting is left alone).
                    if !msg_text.is_empty() {
                        let _ = sink.send(AgentEvent::TextRewind(std::mem::take(&mut msg_text)));
                    }
                    // A new assistant message: bank the previous one's usage.
                    tokens_done += std::mem::take(&mut tokens_cur);
                    streamed_text = false;
                } else if ev_type == "message_delta" {
                    // Cumulative REAL output tokens for the current message.
                    if let Some(t) = v["event"]["usage"]["output_tokens"].as_u64() {
                        tokens_cur = t;
                        let _ = sink.send(AgentEvent::Pulse { chars: 0, tokens: Some(tokens_done + tokens_cur) });
                    }
                }
            }
            // Completed assistant message → tool calls + thinking, plus the text
            // itself when it never streamed (see `streamed_text`).
            if v["type"] == "assistant" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        match b["type"].as_str() {
                            // Normally a no-op: the deltas already streamed this
                            // text and re-sending it would double the reply. But a
                            // turn whose partial stream never arrived carries its
                            // ONLY copy here — without this the entire reply is
                            // dropped and the user gets "(the agent produced no
                            // output)" while the transcript holds a full answer.
                            Some("text") if !streamed_text => {
                                if let Some(t) = b["text"].as_str() {
                                    if !t.trim().is_empty() {
                                        let _ = sink.send(AgentEvent::Text(t.to_string()));
                                        produced = true;
                                    }
                                }
                            }
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
                // This message LANDED — whatever it streamed is final, and the
                // next `message_start` is a genuinely new message, not a retry.
                msg_text.clear();
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
                // Not OUR receipt. `claude -p --resume` drains the session's
                // input QUEUE before it looks at the prompt we came here with,
                // and a turn the user stopped leaves an item in that queue (a
                // `<task-notification>` saying a background shell has no
                // completion record). Claude closes that queued item as a turn
                // of its own — no model call, so zero usage, empty `result`,
                // ~100ms — emits a `result` for it, and only THEN dequeues our
                // prompt and starts working on the real answer.
                //
                // Breaking on that receipt is the "0.1s empty reply" the field
                // kept hitting: the renderer flushed a bare
                // `{% mafold/result duration="0.1s" %}`, the real answer was
                // streamed into a pipe with no reader left, and `child.wait()`
                // below then blocked forever on a process that was still
                // working — so the draft was never finalized and NEITHER retry
                // in `agent::handle` ever got to run. Step over the receipt and
                // keep reading; our own result is still coming.
                if is_queued_receipt(&v, produced) && skipped_receipts < MAX_SKIPPED_RECEIPTS {
                    skipped_receipts += 1;
                    continue;
                }
                // Last-resort carrier: a turn that succeeded but reached us with
                // NOTHING (no deltas, no assistant message, no tool events) still
                // has its final answer here. Delivering it beats finalizing an
                // empty bubble — the failure this guards against is silent, and
                // the daemon's empty-turn retry would otherwise burn a second
                // full turn just to lose the reply again.
                if !produced {
                    if let Some(t) = v["result"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        let _ = sink.send(AgentEvent::Text(t.to_string()));
                        produced = true;
                    }
                }
                let toks = usage_tokens(&v);
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
        // `claude` normally exits within a beat of its final `result`. When it
        // does NOT — more queued input behind us, a background task it is still
        // winding down — an unbounded wait PARKS THE WHOLE TURN here: the reply
        // is never finalized (a draft that stays open forever), the caller's
        // retries never run, and the child lives on writing into a pipe with no
        // reader until it deadlocks on a full pipe buffer, still holding the
        // session and whatever tools it spawned. Two such orphans were alive on
        // the field machine when this was diagnosed — one of them still starting
        // shells 13 minutes after its "reply" had been rendered.
        //
        // We already have this turn's result, so an overstaying child is not a
        // failure of the reply: give it a grace period, then kill it and return
        // what we have.
        const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(20);
        let status = match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                if let Some(t) = stderr_task { t.abort(); }
                return Ok(TurnOutcome { produced, stopped, session: session_id, error });
            }
        };
        if !status.success() {
            // The concurrent reader already drained stderr (no post-wait read that
            // could have deadlocked) — just collect what it captured.
            let err = match stderr_task {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            // Reaching here at all means the process died WITHOUT a terminal
            // `result` line — EVERY `result`, success or `is_error`, breaks the
            // loop above and returns before this point. So this is the silent
            // death: claude quit without saying why on the stream, and (in the
            // cases seen in the field) without saying why on stderr either.
            //
            // NOT `bail!`. A nonzero exit means "the turn ended on an error",
            // which is exactly what `TurnOutcome::error` carries — and only that
            // form reaches the caller's stale-resume recovery, which drops the
            // resumed session and retries ONCE on a fresh one so the user's
            // message still gets answered. An `Err` here bypassed BOTH retry
            // paths in `agent::handle` (they only match `Ok`), so this silent
            // death burned the whole turn on a reply card that lived a couple of
            // seconds and the user had to notice and resend by hand. `produced`
            // rides along, so a run that already streamed work is never redone.
            return Ok(TurnOutcome {
                produced,
                stopped,
                session: session_id,
                error: Some(exit_reason(status.code(), &err, plain_tail.make_contiguous())),
            });
        }
        if let Some(t) = stderr_task { t.abort(); }
        Ok(TurnOutcome { produced, stopped, session: session_id, error: None })
    }

    fn discover(&self, workdir: &str) -> Value {
        crate::discover::all(workdir)
    }

    async fn command(&self, _client: &Client, _chat_id: &str, name: &str, arg: &str, workdir: &str, session: Option<&str>) -> CommandOutcome {
        match crate::commands::handle(name, arg, workdir, session).await {
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

/// The context size a `compact_boundary` event says it compacted. Its own
/// function so the JSON path is pinned by a test against a real captured event —
/// a silently-wrong path here reads exactly like no compaction at all.
fn compaction_pre_tokens(v: &Value) -> Option<u64> {
    v["compactMetadata"]["preTokens"].as_u64()
}

/// The usage-limit state worth relaying from a `rate_limit_event`'s
/// `rate_limit_info`, or None when the limit is healthy. Claude emits one of
/// these on ordinary turns too, so the "allowed" gate is what keeps this from
/// stamping a quota notice onto every single reply — it is load-bearing, not
/// defensive.
fn rate_limit_notice(info: &Value) -> Option<(String, Option<i64>)> {
    if info["status"].as_str().unwrap_or("allowed") == "allowed" {
        return None;
    }
    Some((
        info["rateLimitType"].as_str().unwrap_or("usage").to_string(),
        info["resetsAt"].as_i64(),
    ))
}

/// Keep the last few plain-text stdout lines (see `plain_tail`). Bounded in both
/// line count and line length — these are diagnostic breadcrumbs for a failed
/// run, not a transcript, and a chatty non-JSON stream must not grow them.
fn push_plain(tail: &mut std::collections::VecDeque<String>, line: &str) {
    const MAX_LINES: usize = 5;
    const MAX_CHARS: usize = 300;
    let mut s: String = line.chars().take(MAX_CHARS).collect();
    if line.chars().count() > MAX_CHARS {
        s.push('…');
    }
    tail.push_back(s);
    while tail.len() > MAX_LINES {
        tail.pop_front();
    }
}

/// Why a `claude` run that exited nonzero failed, in the most useful words we
/// have: stderr when claude wrote there, else the plain-text stdout tail, else
/// the exit code itself. Never a bare "exited unsuccessfully" — a failure with
/// no reason attached is unactionable for the user AND undiagnosable from the
/// daemon log, which is how this class of dead reply went unexplained for weeks.
fn exit_reason(code: Option<i32>, stderr: &str, plain_tail: &[String]) -> String {
    const HEAD: &str = "claude exited unsuccessfully";
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return format!("{HEAD}: {stderr}");
    }
    let tail: Vec<&str> = plain_tail.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !tail.is_empty() {
        return format!("{HEAD}: {}", tail.join(" / "));
    }
    match code {
        Some(c) => format!("{HEAD} (exit code {c}, and it printed nothing to stdout or stderr)"),
        None => format!("{HEAD} (killed by a signal, and it printed nothing to stdout or stderr)"),
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

/// Every `usage` field that counts toward a turn's token total.
const USAGE_KEYS: [&str; 4] =
    ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"];

fn usage_tokens(v: &Value) -> u64 {
    let u = &v["usage"];
    USAGE_KEYS.iter().filter_map(|k| u[*k].as_u64()).sum()
}

/// Is this successful `result` the receipt for a message that ISN'T ours — a
/// queued `<task-notification>` claude closed out before it even looked at our
/// prompt? Three things are true of one and of nothing else: this run has
/// produced nothing at all, the result text is empty, and it burned zero tokens
/// (no model call happened, so there is no usage). A real turn always spends
/// input tokens, so a real turn can never look like this.
///
/// `is_error` / non-success subtypes are handled before this is consulted.
fn is_queued_receipt(v: &Value, produced: bool) -> bool {
    !produced
        && usage_tokens(v) == 0
        && v["result"].as_str().map(str::trim).is_none_or(str::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape claude emitted at 13:45:08 on the field machine, right
    /// after a stopped turn left a `<task-notification>` in the session queue.
    /// Breaking the read loop here is what rendered the "0.1s empty reply" and
    /// then parked the turn forever in `child.wait()`.
    #[test]
    fn queued_notification_receipt_is_not_our_turn() {
        let v: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 103, "result": "", "session_id": "e6b17a8d",
            "usage": {"input_tokens": 0, "output_tokens": 0}
        });
        assert!(is_queued_receipt(&v, false), "must be stepped over");
    }

    /// A real answer's receipt must END the turn — never be mistaken for a
    /// queued one, or the reply would hang until the stall watchdog fires.
    #[test]
    fn a_real_turns_receipt_is_ours() {
        let real: Value = serde_json::json!({
            "type": "result", "subtype": "success", "duration_ms": 21000,
            "result": "done", "usage": {"input_tokens": 12000, "output_tokens": 300}
        });
        assert!(!is_queued_receipt(&real, true), "streamed output already proves it is ours");
        assert!(!is_queued_receipt(&real, false), "text + usage prove it is ours");

        // Streaming carried the whole reply, so the final result text is empty —
        // usage still says a model call happened.
        let streamed: Value = serde_json::json!({
            "type": "result", "subtype": "success", "result": "",
            "usage": {"input_tokens": 9000, "cache_read_input_tokens": 400}
        });
        assert!(!is_queued_receipt(&streamed, false), "zero-text but real usage is ours");
    }

    #[test]
    fn usage_tokens_sums_every_counter() {
        let v: Value = serde_json::json!({"usage": {
            "input_tokens": 1, "output_tokens": 2,
            "cache_read_input_tokens": 4, "cache_creation_input_tokens": 8
        }});
        assert_eq!(usage_tokens(&v), 15);
        assert_eq!(usage_tokens(&serde_json::json!({})), 0);
    }

    /// The three 0.0s cards in the field carried this on stderr — that path
    /// already worked and must keep working.
    #[test]
    fn stderr_is_the_reason_when_claude_writes_there() {
        let r = exit_reason(Some(1), "No conversation found with session ID: 29cbfee1\n", &[]);
        assert!(r.contains("No conversation found"), "{r}");
    }

    /// The regression this fixes: claude printed the reason as PLAIN TEXT on
    /// stdout, the stream-json parser dropped it as unparseable, stderr was
    /// empty — and the user got a bare "claude exited unsuccessfully" with
    /// nothing to act on and nothing in the log to diagnose.
    #[test]
    fn plain_stdout_tail_is_the_reason_when_stderr_is_empty() {
        let tail = vec!["Claude usage limit reached|1785900000".to_string()];
        let r = exit_reason(Some(1), "   \n ", &tail);
        assert!(r.contains("usage limit reached"), "{r}");
    }

    /// Even with nothing on either stream, the exit code is real information —
    /// it used to be discarded too.
    #[test]
    fn exit_code_is_reported_when_there_is_no_output_at_all() {
        let r = exit_reason(Some(143), "", &[]);
        assert!(r.contains("143"), "{r}");
        assert!(exit_reason(None, "", &[]).contains("signal"));
    }

    #[test]
    fn plain_tail_is_bounded_in_lines_and_line_length() {
        let mut t = std::collections::VecDeque::new();
        for i in 0..20 {
            push_plain(&mut t, &format!("line {i}"));
        }
        assert_eq!(t.len(), 5, "keeps only the tail");
        assert_eq!(t.back().unwrap(), "line 19", "keeps the LAST lines, not the first");
        push_plain(&mut t, &"x".repeat(1000));
        assert!(t.back().unwrap().chars().count() <= 301, "long line truncated");
    }

    /// Blank-only breadcrumbs must not masquerade as an explanation.
    #[test]
    fn whitespace_only_tail_falls_through_to_the_exit_code() {
        let r = exit_reason(Some(2), "", &["".into(), "   ".into()]);
        assert!(r.contains("exit code 2"), "{r}");
    }

    /// A real auto-compaction event, verbatim from a transcript on disk. Pins
    /// the JSON path — read the wrong one and compaction goes back to being
    /// invisible, with nothing failing to say so.
    #[test]
    fn pre_tokens_come_from_a_real_compact_boundary_event() {
        let v: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted",
                "isMeta":false,"level":"info",
                "compactMetadata":{"trigger":"auto","preTokens":302336,"durationMs":135931,
                                   "preCompactDiscoveredTools":["WebFetch","WebSearch"]}}"#,
        )
        .unwrap();
        assert_eq!(compaction_pre_tokens(&v), Some(302336));
    }

    /// The event carries no post-compaction count, so an absent `preTokens`
    /// must degrade to "compacted, size unknown" rather than to a zero that a
    /// caller could mistake for a real measurement.
    #[test]
    fn a_compaction_event_without_pre_tokens_is_none_not_zero() {
        let v: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}"#,
        )
        .unwrap();
        assert_eq!(compaction_pre_tokens(&v), None);
    }

    /// A real healthy rate-limit event, verbatim off the stream. Claude sends
    /// one of these on ordinary turns — relaying it would stamp a quota notice
    /// onto every reply.
    #[test]
    fn a_healthy_rate_limit_is_not_relayed() {
        let v: Value = serde_json::from_str(
            r#"{"status":"allowed","resetsAt":1785901800,"rateLimitType":"five_hour",
                "overageStatus":"rejected","isUsingOverage":false}"#,
        )
        .unwrap();
        assert_eq!(rate_limit_notice(&v), None);
    }

    #[test]
    fn an_exhausted_rate_limit_is_relayed_with_its_kind_and_reset() {
        let v: Value = serde_json::from_str(
            r#"{"status":"rejected","resetsAt":1785901800,"rateLimitType":"five_hour"}"#,
        )
        .unwrap();
        assert_eq!(rate_limit_notice(&v), Some(("five_hour".into(), Some(1785901800))));
    }

    /// An unfamiliar shape must not be silently swallowed: anything that isn't
    /// explicitly "allowed" is worth telling the user about.
    #[test]
    fn an_unrecognized_rate_limit_status_is_still_relayed() {
        let v: Value = serde_json::from_str(r#"{"status":"something_new"}"#).unwrap();
        assert_eq!(rate_limit_notice(&v), Some(("usage".into(), None)));
    }
}
