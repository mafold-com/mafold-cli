//! Codex harness — drives OpenAI's local `codex` CLI headlessly
//! (`codex exec --json`) and normalizes its thread/item JSONL event stream into
//! [`AgentEvent`]s. The renderer (`crate::render`) turns those into the same chat
//! text + cards it produces for every other harness, so card rendering is
//! identical — this file only has to speak Codex's dialect.
//!
//! Differences from Claude Code that shape this impl:
//! - **No `--append-system-prompt`.** Codex has no system-prompt flag, so the
//!   daemon's mafold preamble (identity / conversation / embeddable cards) is
//!   folded into the front of the prompt instead.
//! - **Block-level streaming, not token-level.** Codex `--json` emits complete
//!   `item.completed` events (a whole assistant message, a whole reasoning block)
//!   rather than per-token deltas. The transcript is still interleaved in arrival
//!   order (§8) — narration, tool cards, results — just at item granularity.
//! - **Reasoning effort IS its thinking.** Codex has no separate extended-thinking
//!   budget, so the chat's `/think` budget is ignored here; depth is controlled by
//!   the owner-set effort (mapped to `model_reasoning_effort`).
//! - **Auth lives on the host.** Codex uses its own login (`codex login`, or a
//!   `CODEX_API_KEY` / `OPENAI_API_KEY` in the environment); we don't strip it.
//! - **Generated images never appear on the stream.** See [`ImageSweep`] — the
//!   only harness dialect we have to read off the filesystem instead.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentEvent, CommandOutcome, Harness, Turn, TurnOutcome};
use crate::client::Client;

pub struct Codex;

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn available(&self) -> bool {
        super::on_path("codex")
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        // `thinking` and `ask_file` don't apply to Codex (no extended-thinking
        // budget; no AskUserQuestion tool / PreToolUse hook) — accepted and
        // ignored. `surface` likewise: the bash-hook that detaches background
        // tasks is only wired for Claude Code, so nothing here registers any.
        // `steer_file` too: with no hook to drain it mid-turn, `can_steer()`
        // stays false and the daemon delivers a mid-turn message as the
        // FOLLOW-UP turn instead — never dropped, just later, and the user is
        // told which of the two they got.
        let Turn {
            prompt,
            workdir,
            session,
            model,
            effort,
            thinking: _,
            cancel,
            system,
            ask_file: _,
            steer_file: _,
            conv,
            surface: _,
            draft,
        } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }

        // Codex has no system-prompt flag; fold the mafold preamble into the
        // prompt so a mafold-unaware agent still knows it's acting as this bot.
        let full_prompt = match &system {
            Some(sys) if !sys.trim().is_empty() => format!("{sys}\n\n---\n\n{prompt}"),
            _ => prompt,
        };

        let program = super::program("codex");
        let p = RunParams {
            program: &program,
            full_prompt: &full_prompt,
            workdir: &workdir,
            model: model.as_deref(),
            effort: effort.as_deref(),
            conv: &conv,
            draft: &draft,
            cancel: &cancel,
            sink: &sink,
        };
        match run_once(&p, session.as_deref()).await {
            // The stored thread id can be stale or foreign — Codex expires
            // rollouts, and a conversation may carry a session written by a
            // DIFFERENT harness (a bot switched to codex mid-conversation).
            // Retry once WITHOUT resume: the failed attempt exits before
            // emitting any event, so the fresh run streams into a clean turn.
            Err(e) if session.is_some() && is_stale_thread(&e) => run_once(&p, None).await,
            r => r,
        }
    }

    fn discover(&self, _workdir: &str) -> Value {
        // Codex custom prompts (`~/.codex/prompts/*.md`) are a TUI feature — a
        // forwarded `/name` wouldn't resolve in headless `codex exec` (it lands as
        // literal prompt text), so publishing them would only add dead menu
        // entries. The daemon's own control commands (/clear /new /model /status …)
        // are added separately and DO work.
        Value::Array(vec![])
    }

    async fn command(&self, _client: &Client, _chat_id: &str, _name: &str, _arg: &str, _workdir: &str, _session: Option<&str>) -> CommandOutcome {
        // No emulated slash commands yet — anything that isn't a daemon control
        // command is forwarded to `codex exec` as a prompt.
        CommandOutcome::Forward
    }

    async fn status_line(&self) -> String {
        auth_status_line().await
    }

    async fn cli_version(&self) -> String {
        codex_version().await
    }
}

/// Borrowed per-turn invocation parameters shared by the resume attempt and its
/// fresh-thread retry.
#[derive(Clone, Copy)]
struct RunParams<'a> {
    /// The `codex` binary to spawn — resolved once by [`Harness::run`]. A
    /// parameter rather than a lookup inside the run so the tests below can
    /// drive the whole event loop against a scripted stream.
    program: &'a std::ffi::OsStr,
    full_prompt: &'a str,
    workdir: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    conv: &'a str,
    draft: &'a str,
    cancel: &'a std::sync::Arc<tokio::sync::Notify>,
    sink: &'a UnboundedSender<AgentEvent>,
}

/// A resume failure that means "this thread id is unusable" (expired rollout, or
/// a session written by another harness) — retry fresh, don't surface.
fn is_stale_thread(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("thread/resume") || s.contains("no rollout found")
}

/// The `codex exec` argv for one turn — everything EXCEPT the prompt, which is
/// fed on stdin. That is what the trailing `-` means: both `codex exec [PROMPT]`
/// and `codex exec resume <ID> [PROMPT]` document it as "read the instructions
/// from stdin".
///
/// **No argument here may ever contain a newline**, which is why the prompt isn't
/// one. It is always multi-line (the mafold preamble is joined to the message
/// with `\n\n---\n\n`) and it grows without bound (it carries the conversation) —
/// the two things Windows refuses to spawn:
/// - an npm-installed codex is `%APPDATA%\npm\codex.cmd`, a BATCH FILE, and since
///   the BatBadBut fix (CVE-2024-24576) Rust's std refuses to spawn one with any
///   argument containing `\r` or `\n`: `InvalidInput: batch file arguments are
///   invalid`. On argv, every Codex turn on a stock Windows install therefore
///   died before the process even started — and the error named nothing an owner
///   could act on. (Reported from the field; see the test below.)
/// - a command line is hard-capped at 32,767 UTF-16 units, so a long enough chat
///   makes `CreateProcessW` refuse the spawn outright (os error 206).
///
/// Both disappear when the prompt is stdin; Claude Code's harness feeds its own
/// prompt that way for the same reasons.
fn exec_args(session: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    // Resume the conversation's Codex thread for context. The subcommand form
    // is `codex exec resume <THREAD_ID> [options] [prompt]`; options and the
    // prompt still parse after it.
    if let Some(sid) = session {
        args.push("resume".into());
        args.push(sid.into());
    }
    args.push("--json".into());
    // The daemon already gates WHO can drive the bot (allow-list), exactly as it
    // does for Claude Code's `--dangerously-skip-permissions`. So run Codex with
    // full autonomy — no approval prompts (which would hang a headless run), no
    // sandbox.
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    // Bot workdirs aren't necessarily git repos; Codex otherwise refuses.
    args.push("--skip-git-repo-check".into());
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    // Reasoning effort (owner-set via Customization) → Codex's config key.
    // Codex supports minimal/low/medium/high; the higher mafold tiers clamp to
    // high. No mapping = Codex's own default.
    if let Some(eff) = effort.and_then(map_effort) {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort=\"{eff}\""));
    }
    // `--` stops flag parsing so the `-` after it is read as the prompt argument.
    args.push("--".into());
    args.push("-".into());
    args
}

/// One `codex exec` invocation (optionally resuming `session`), streaming
/// normalized events into the sink.
async fn run_once(p: &RunParams<'_>, session: Option<&str>) -> Result<TurnOutcome> {
    let RunParams { program, full_prompt, workdir, model, effort, conv, draft, cancel, sink } = *p;

    let mut cmd = tokio::process::Command::new(program);
        cmd.args(exec_args(session, model, effort));
        // Export the current conversation so `mafold room …` targets THIS room
        // (harmless for Codex, which has no room skill today — kept for parity).
        cmd.env("MAFOLD_CONV", conv);
        // The reply being streamed right now — `mafold attach <file>` hangs
        // media on it. Codex's own generated images are swept up automatically
        // (see ImageSweep); this is the door for everything else it draws.
        cmd.env("MAFOLD_DRAFT", draft);
        cmd.kill_on_drop(true);
        // Don't let the console child flash a window (the agent runs detached).
        crate::platform::no_window(&mut cmd);

        let mut child = cmd
            .current_dir(workdir)
            // PIPED, never null or inherited: the prompt goes in HERE — `exec_args`
            // ends with the `-` that tells codex to read it from stdin, and nothing
            // else on the command line ever wants stdin.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| super::spawn_err("codex", workdir, e))?;
        // Register this run in the live-children set so a daemon shutdown kills
        // exactly THIS process (see harness::live_children) — same contract as
        // the Claude harness; RAII deregisters on every exit path.
        let _child_guard = crate::harness::ChildGuard::new(child.id());

        // Feed the prompt in its OWN task (same shape as the Claude harness): a
        // prompt past the pipe buffer (~64KB — and this one holds the conversation)
        // would otherwise block us here while codex is blocked writing stdout that
        // nobody is reading yet. Dropping the handle closes stdin, which is the EOF
        // the `-` waits for.
        if let Some(mut si) = child.stdin.take() {
            let prompt = full_prompt.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = si.write_all(prompt.as_bytes()).await;
                let _ = si.shutdown().await;
            });
        }

        let stdout = child.stdout.take().context("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();

        // Drain stderr CONCURRENTLY (same deadlock guard as the Claude harness): a
        // blocked stderr pipe would stall the turn while it holds the conv lock.
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
        let mut error: Option<String> = None;
        let mut images: Option<ImageSweep> = None;

        // Emit whatever `image_gen` has written since the last check. Called
        // after every completed item (so a picture reaches the bubble while the
        // turn is still running, not in a lump at the end) and once more before
        // `Done`, which the render loop treats as terminal.
        macro_rules! sweep_images {
            () => {
                if let Some(sw) = images.as_mut() {
                    for path in sw.take_new() {
                        let _ = sink.send(AgentEvent::Image { path });
                        produced = true;
                    }
                }
            };
        }

        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line? { Some(l) => l, None => break },
                _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match v["type"].as_str().unwrap_or("") {
                // The thread id — our resumable session for this conversation.
                "thread.started" => {
                    if let Some(t) = v["thread_id"].as_str() {
                        if session_id.is_none() {
                            session_id = Some(t.to_string());
                        }
                        // Baseline BEFORE any item can run, so this turn's
                        // sweeps see only this turn's images.
                        images = Some(ImageSweep::baseline(t));
                        let _ = sink.send(AgentEvent::Session(t.to_string()));
                    }
                }
                // One `codex exec` run = one turn; its completion ends the stream.
                "turn.completed" => {
                    // A turn that COMPLETED is a success, whatever it had to
                    // survive on the way — drop any retry notice taken below.
                    error = None;
                    sweep_images!(); // must precede Done — the renderer stops there
                    let u = &v["usage"];
                    let toks: u64 = ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens"]
                        .iter()
                        .filter_map(|k| u[*k].as_u64())
                        .sum();
                    let _ = sink.send(AgentEvent::Done {
                        duration_ms: None,
                        cost_usd: None,
                        tokens: if toks > 0 { Some(toks) } else { None },
                    });
                    break;
                }
                // Model gave up mid-turn (stream ended, etc.) — surface + stop.
                "turn.failed" => {
                    error = Some(err_text(&v["error"]).unwrap_or_else(|| "the turn failed".into()));
                    let _ = child.start_kill();
                    break;
                }
                // NOT fatal on its own, however much it reads like it. Codex
                // streams its RETRY NOTICES through `error` — "Reconnecting... 2/5
                // (stream disconnected before completion: …)" comes straight out
                // of its `core/src/responses_retry.rs`, as does "Falling back from
                // WebSockets to HTTPS transport." — and then it carries on: up to
                // five notices, after which the turn either completes normally or
                // ends with a bare `error` + a `turn.failed`.
                //
                // Killing the child on the first one (what this did) shot codex
                // MID-RECONNECT: every transient blip — one proxy hiccup is enough
                // — became "⚠️ Agent stopped: Reconnecting... 2/5" on a turn that
                // would have finished by itself. Worse, `agent.rs` reads a turn
                // error on a RESUMED session as "this thread is corrupt" and drops
                // the session, so the next message lost the conversation too.
                //
                // So: remember it and let the STREAM decide. `turn.completed`
                // clears it, `turn.failed` overwrites it with the real reason, and
                // if the stream just ends this is the last word we had.
                "error" => {
                    error = Some(
                        v["message"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    );
                }
                phase @ ("item.started" | "item.updated" | "item.completed") => {
                    handle_item(phase, &v["item"], sink, &mut produced);
                    if phase == "item.completed" {
                        sweep_images!();
                    }
                }
                _ => {}
            }
        }
        // (The owned sender lives in `run()` — the channel closes when it returns,
        // and the renderer flushes its tail then.)

        if stopped || error.is_some() {
            let _ = child.start_kill(); // idempotent
            let _ = child.wait().await; // reap
            if let Some(t) = stderr_task {
                t.abort();
            }
            return Ok(TurnOutcome {
                produced,
                stopped,
                session: session_id,
                error,
            });
        }
        let status = child.wait().await?;
        if !status.success() {
            let err = match stderr_task {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            let err = err.trim();
            bail!(
                "codex exited unsuccessfully{}",
                if err.is_empty() {
                    String::new()
                } else {
                    format!(": {err}")
                }
            );
        }
        if let Some(t) = stderr_task {
            t.abort();
        }
        Ok(TurnOutcome {
            produced,
            stopped,
            session: session_id,
            error: None,
        })
}

/// Where Codex keeps its state (`CODEX_HOME`, else `~/.codex`) — the same
/// resolution `codex` itself does.
fn codex_home() -> PathBuf {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".codex")
}

/// Watches for images Codex's `image_gen` tool produced this turn.
///
/// It has to be a directory watch, because the picture is **not on the event
/// stream at all**: `codex exec --json` speaks a fixed item vocabulary
/// (`agent_message` / `reasoning` / `command_execution` / `file_change` /
/// `mcp_tool_call` / `web_search` / `todo_list`) with no image member. The
/// generated bytes go straight into the model's own context and to
/// `$CODEX_HOME/generated_images/<thread-id>/<call-id>.png` — so the model
/// sees the image, says "已生成", and everything downstream of it sees nothing.
/// That is the whole bug this exists to close.
///
/// BASELINED at `thread.started`, before the turn can write anything: a resumed
/// thread's directory already holds every image from previous turns, and
/// without a baseline the first resumed turn would re-send all of them.
struct ImageSweep {
    dir: PathBuf,
    seen: HashSet<OsString>,
}

impl ImageSweep {
    /// Start watching `thread_id`'s image directory, treating whatever is
    /// already there as old news.
    fn baseline(thread_id: &str) -> Self {
        let dir = codex_home().join("generated_images").join(thread_id);
        let mut s = Self { dir, seen: HashSet::new() };
        s.take_new(); // prime `seen`; earlier turns' output is not ours to send
        s
    }

    /// Images that appeared since the last call, oldest first (so a turn that
    /// draws several sends them in the order they were made). Missing dir — the
    /// overwhelmingly common case, since most turns draw nothing — is not an
    /// error, just an empty sweep.
    fn take_new(&mut self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut fresh: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            if self.seen.contains(&name) {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            self.seen.insert(name);
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            fresh.push((mtime, e.path()));
        }
        fresh.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        fresh.into_iter().map(|(_, p)| p).collect()
    }
}

/// Map a mafold effort level (`low`…`max`) to Codex's `model_reasoning_effort`
/// (`minimal`/`low`/`medium`/`high`). `None` = use Codex's own default.
fn map_effort(effort: &str) -> Option<&'static str> {
    match effort.trim().to_lowercase().as_str() {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        // Codex tops out at `high`; the higher mafold tiers clamp to it.
        "high" | "xhigh" | "max" => Some("high"),
        _ => None, // "default"/unknown → Codex default
    }
}

/// Normalize a Codex `item` event into `AgentEvent`s. `phase` is the outer
/// `item.started` / `item.updated` / `item.completed`. `item.id` correlates a
/// command's start (the tool card) with its completion (the output).
fn handle_item(phase: &str, item: &Value, sink: &UnboundedSender<AgentEvent>, produced: &mut bool) {
    let id = item["id"].as_str().unwrap_or("").to_string();
    let itype = item["type"].as_str().unwrap_or("");
    let completed = phase == "item.completed";

    match itype {
        // The assistant's reply text (whole block, at completion).
        "agent_message" if completed => {
            if let Some(t) = item["text"].as_str() {
                if !t.is_empty() {
                    let _ = sink.send(AgentEvent::Text(t.to_string()));
                    *produced = true;
                }
            }
        }
        // A reasoning / chain-of-thought block (collapsed in the UI).
        "reasoning" if completed => {
            if let Some(t) = item["text"].as_str() {
                if !t.trim().is_empty() {
                    let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                    *produced = true;
                }
            }
        }
        // A shell command: the card at start, its output at completion. Named
        // "bash" so the result renders as a `{% bash %}` card, like Claude's Bash.
        "command_execution" => {
            if phase == "item.started" {
                let _ = sink.send(AgentEvent::ToolCall {
                    id,
                    name: "bash".into(),
                    input: json!({ "command": command_str(&item["command"]) }),
                });
                *produced = true;
            } else if completed {
                let mut text = item["aggregated_output"].as_str().unwrap_or("").to_string();
                if let Some(code) = item["exit_code"].as_i64() {
                    if code != 0 {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&format!("(exit {code})"));
                    }
                }
                let _ = sink.send(AgentEvent::ToolResult { id, text });
            }
        }
        // File edits (Codex's apply_patch). We only know the paths + kind, not the
        // hunks, so render one clean per-file tool card (counted as an edit) rather
        // than a fake 0/0 diff. The actual patch, when applied via a shell
        // command, still shows through the command_execution card above.
        "file_change" if completed => {
            if let Some(changes) = item["changes"].as_array() {
                for (i, ch) in changes.iter().enumerate() {
                    let path = ch["path"].as_str().unwrap_or("");
                    let kind = ch["kind"].as_str().unwrap_or("update");
                    let _ = sink.send(AgentEvent::ToolCall {
                        id: format!("{id}-{i}"),
                        name: "apply_patch".into(),
                        input: json!({ "file_path": format!("{path} ({kind})") }),
                    });
                    *produced = true;
                }
            }
        }
        // An MCP tool call: the invocation card + its result text.
        "mcp_tool_call" => {
            if phase == "item.started" {
                let name = format!(
                    "{}.{}",
                    item["server"].as_str().unwrap_or("mcp"),
                    item["tool"].as_str().unwrap_or("tool"),
                );
                let _ = sink.send(AgentEvent::ToolCall {
                    id,
                    name,
                    input: item["arguments"].clone(),
                });
                *produced = true;
            } else if completed {
                let text = mcp_result_text(item);
                if !text.is_empty() {
                    let _ = sink.send(AgentEvent::ToolResult { id, text });
                }
            }
        }
        // A web search → the `{% web query=… %}` card (name mirrors Claude's).
        "web_search" if completed => {
            let _ = sink.send(AgentEvent::ToolCall {
                id,
                name: "websearch".into(),
                input: json!({ "query": item["query"].as_str().unwrap_or("") }),
            });
            *produced = true;
        }
        // The plan → a `{% todo %}` card (name mirrors Claude's TodoWrite). Emitted
        // once, at completion, to avoid a card per intermediate update.
        "todo_list" if completed => {
            let todos: Vec<Value> = item["items"].as_array().map(|arr| {
                arr.iter().map(|t| json!({
                    "content": t["text"].as_str().unwrap_or(""),
                    "status": if t["completed"].as_bool().unwrap_or(false) { "completed" } else { "pending" },
                })).collect()
            }).unwrap_or_default();
            let _ = sink.send(AgentEvent::ToolCall {
                id,
                name: "todowrite".into(),
                input: json!({ "todos": todos }),
            });
            *produced = true;
        }
        // A non-fatal item warning (e.g. truncated output) — surface, don't hide.
        "error" if completed => {
            if let Some(m) = item["message"].as_str() {
                if !m.trim().is_empty() {
                    let _ = sink.send(AgentEvent::Text(format!("\n> ⚠️ {}\n", m.trim())));
                    *produced = true;
                }
            }
        }
        _ => {}
    }
}

/// Codex's `command` is usually a string (`"bash -lc ls"`) but can be an argv
/// array; normalize both to a single displayable string.
fn command_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Join an mcp_tool_call result's text content blocks; fall back to its error.
fn mcp_result_text(item: &Value) -> String {
    if let Some(blocks) = item["result"]["content"].as_array() {
        let text = blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(e) = item["error"]["message"]
        .as_str()
        .or_else(|| item["error"].as_str())
    {
        if !e.trim().is_empty() {
            return format!("error: {e}");
        }
    }
    String::new()
}

/// An error object's message — `{message: "..."}` or a bare string.
fn err_text(e: &Value) -> Option<String> {
    e["message"]
        .as_str()
        .or_else(|| e.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// One-line auth status for the `/status` Account row: how the host is
/// authenticated (ChatGPT login vs API key), read from `$CODEX_HOME/auth.json`
/// (default `~/.codex`). The CLI version is reported separately on the Harness
/// row via `cli_version()`, so it isn't repeated here (parallel to Claude Code,
/// whose Account row is auth-only). Empty if Codex isn't installed.
async fn auth_status_line() -> String {
    // Gate on Codex being installed at all — otherwise this row is just noise.
    if codex_version().await.is_empty() {
        return String::new();
    }
    auth_mode().unwrap_or_else(|| "not logged in".into())
}

/// `codex --version` → "0.5.0" (first numeric-ish token; "" if the CLI is missing).
async fn codex_version() -> String {
    use std::time::Duration;
    let mut cmd = tokio::process::Command::new(super::program("codex"));
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

/// Read `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) and report the
/// auth mode without exposing any secret: a ChatGPT login (`tokens`) vs an API
/// key (`OPENAI_API_KEY`). None = no auth file.
fn auth_mode() -> Option<String> {
    let home = std::env::var("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".codex")
        });
    let text = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if v.get("tokens").is_some() {
        Some("ChatGPT login".into())
    } else if v
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.is_empty())
    {
        Some("API key".into())
    } else {
        Some("configured".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `codex` stand-in that prints `stream` on stdout, one JSON event per
    /// line, and exits 0 — enough to drive the whole event loop.
    fn scripted_codex(dir: &std::path::Path, stream: &[&str]) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("codex.cmd");
            let mut s = String::from("@echo off\r\n");
            for line in stream {
                s.push_str(&format!("echo {line}\r\n"));
            }
            std::fs::write(&path, s).unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("codex.sh");
            let mut s = String::from("#!/bin/sh\n");
            for line in stream {
                s.push_str(&format!("cat <<'MAFOLD_JSON'\n{line}\nMAFOLD_JSON\n"));
            }
            std::fs::write(&path, s).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    /// Drive one turn against a scripted stream.
    async fn run_scripted(tag: &str, stream: &[&str]) -> TurnOutcome {
        let dir = std::env::temp_dir().join(format!("mafold-codex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = scripted_codex(&dir, stream);
        let workdir = dir.to_string_lossy().to_string();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let (sink, _rx) = tokio::sync::mpsc::unbounded_channel();
        let out = run_once(
            &RunParams {
                program: program.as_os_str(),
                full_prompt: "hi",
                workdir: &workdir,
                model: None,
                effort: None,
                conv: "conv",
                draft: "draft",
                cancel: &cancel,
                sink: &sink,
            },
            None,
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// Codex streams RETRY NOTICES through `error` events ("Reconnecting... 2/5
    /// …", from its `core/src/responses_retry.rs`) and then carries on. This
    /// harness used to kill the child on the first one, so a single proxy hiccup
    /// became "⚠️ Agent stopped: Reconnecting... 2/5" on a turn codex was still
    /// finishing — and `agent.rs` dropped the resumed session on top of it.
    /// A turn that reconnects and completes is a SUCCESS.
    #[tokio::test]
    async fn a_reconnect_notice_does_not_end_the_turn() {
        let out = run_scripted(
            "reconnect",
            &[
                r#"{"type":"thread.started","thread_id":"01a06fa7-61a1-7871-adaf-7410e6e063e3"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"error","message":"Reconnecting... 1/5 stream disconnected before completion"}"#,
                r#"{"type":"error","message":"Reconnecting... 2/5 stream disconnected before completion"}"#,
                r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
                r#"{"type":"turn.completed","usage":{"output_tokens":5}}"#,
            ],
        )
        .await;
        assert_eq!(out.error, None, "a completed turn must not carry a retry notice");
        assert!(out.produced, "the message it recovered to send must survive");
        assert_eq!(out.session.as_deref(), Some("01a06fa7-61a1-7871-adaf-7410e6e063e3"));
    }

    /// …and when the retries really are exhausted, the LAST word wins: codex's
    /// own reason from `turn.failed`, not the "Reconnecting... 5/5" banner that
    /// happens to precede it.
    #[tokio::test]
    async fn an_exhausted_retry_fails_with_codex_s_reason() {
        let out = run_scripted(
            "exhausted",
            &[
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                r#"{"type":"error","message":"Reconnecting... 5/5 stream disconnected before completion"}"#,
                r#"{"type":"error","message":"stream disconnected before completion: Transport error"}"#,
                r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion: Transport error"}}"#,
            ],
        )
        .await;
        let err = out.error.expect("an exhausted retry must still fail the turn");
        assert!(err.contains("stream disconnected"), "{err}");
        assert!(!err.contains("Reconnecting"), "the banner is not the reason: {err}");
    }

    /// A stream that dies mid-turn without saying why still has to say SOMETHING:
    /// the last notice is the only word we had.
    #[tokio::test]
    async fn a_stream_that_just_stops_surfaces_its_last_notice() {
        let out = run_scripted(
            "eof",
            &[
                r#"{"type":"thread.started","thread_id":"t-2"}"#,
                r#"{"type":"error","message":"Reconnecting... 3/5 stream disconnected before completion"}"#,
            ],
        )
        .await;
        assert_eq!(
            out.error.as_deref(),
            Some("Reconnecting... 3/5 stream disconnected before completion")
        );
    }

    /// The invariant that keeps this harness alive on Windows: the prompt rides
    /// stdin (argv ends with `--` `-`), and nothing on the command line carries a
    /// newline — see [`exec_args`]. Resume path included: it takes the same
    /// trailing prompt argument.
    #[test]
    fn the_prompt_never_rides_argv() {
        for session in [None, Some("01a06b89-3f5e-7e21-b3dc-f0a9c61ffb73")] {
            let args = exec_args(session, Some("gpt-5-codex"), Some("xhigh"));
            assert!(
                !args.iter().any(|a| a.contains('\n') || a.contains('\r')),
                "a newline in argv is unspawnable against a .cmd: {args:?}"
            );
            assert_eq!(args.last().unwrap(), "-", "prompt must come from stdin: {args:?}");
            assert_eq!(args[args.len() - 2], "--", "{args:?}");
        }
    }

    /// The field regression, hermetically: an npm-installed codex is a BATCH FILE
    /// (`%APPDATA%\npm\codex.cmd`), and Rust's std refuses to spawn one with an
    /// argument containing a newline. With the prompt on argv this exact spawn
    /// failed with `batch file arguments are invalid` — no process, no turn, on
    /// every stock Windows install. It must stay spawnable.
    #[cfg(windows)]
    #[test]
    fn a_batch_file_codex_is_still_spawnable() {
        let dir = std::env::temp_dir().join(format!("mafold-batspawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bat = dir.join("codex.cmd");
        // Echoes its own argv back, so this also covers the quoting cmd.exe does
        // to `-c model_reasoning_effort="high"` on the way through.
        std::fs::write(&bat, "@echo off\r\necho %*\r\n").unwrap();

        let out = std::process::Command::new(&bat)
            .args(exec_args(None, None, Some("high")))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut ch| {
                use std::io::Write;
                // The shape of a real prompt: preamble + separator + message.
                ch.stdin.take().unwrap().write_all(b"You are a bot.\n\n---\n\nhi")?;
                ch.wait_with_output()
            })
            .expect("a .cmd codex must still spawn");
        let echoed = String::from_utf8_lossy(&out.stdout);
        assert!(echoed.contains("--json"), "argv mangled: {echoed}");
        assert!(echoed.contains("model_reasoning_effort"), "argv mangled: {echoed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn effort_mapping() {
        assert_eq!(map_effort("low"), Some("low"));
        assert_eq!(map_effort("Medium"), Some("medium"));
        assert_eq!(map_effort("high"), Some("high"));
        assert_eq!(map_effort("xhigh"), Some("high"));
        assert_eq!(map_effort("max"), Some("high"));
        assert_eq!(map_effort("minimal"), Some("minimal"));
        assert_eq!(map_effort("default"), None);
        assert_eq!(map_effort("wat"), None);
    }

    /// A sweep must report only what THIS turn drew. A resumed thread's
    /// directory already holds every image from every earlier turn, so without
    /// the baseline the first resumed turn re-sends the whole history.
    #[test]
    fn a_sweep_reports_only_images_written_after_the_baseline() {
        let dir = std::env::temp_dir().join(format!("mafold-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.png"), b"x").unwrap();

        // Baseline over the pre-existing file (bypasses codex_home for the test).
        let mut sw = ImageSweep { dir: dir.clone(), seen: HashSet::new() };
        sw.take_new();
        assert!(sw.take_new().is_empty(), "a quiet turn sweeps up nothing");

        std::fs::write(dir.join("new.png"), b"y").unwrap();
        let got = sw.take_new();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].file_name().unwrap(), "new.png");
        // Already reported — a later sweep in the same turn must not repeat it.
        assert!(sw.take_new().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Most turns draw nothing, so the directory usually doesn't exist. That is
    /// the normal path, not an error.
    #[test]
    fn sweeping_a_directory_that_was_never_created_is_empty() {
        let mut sw = ImageSweep {
            dir: std::env::temp_dir().join("mafold-sweep-does-not-exist"),
            seen: HashSet::new(),
        };
        assert!(sw.take_new().is_empty());
    }

    #[test]
    fn command_str_string_or_array() {
        assert_eq!(command_str(&json!("bash -lc ls")), "bash -lc ls");
        assert_eq!(command_str(&json!(["bash", "-lc", "ls"])), "bash -lc ls");
        assert_eq!(command_str(&json!(null)), "");
    }

    #[test]
    fn mcp_text_prefers_content_then_error() {
        let ok = json!({ "result": { "content": [{"type":"text","text":"a"},{"type":"text","text":"b"}] } });
        assert_eq!(mcp_result_text(&ok), "a\nb");
        let err = json!({ "result": { "content": [] }, "error": { "message": "boom" } });
        assert_eq!(mcp_result_text(&err), "error: boom");
        assert_eq!(mcp_result_text(&json!({})), "");
    }

    // Collect every AgentEvent a handler run emits (drain a sync channel).
    fn drain(f: impl FnOnce(&UnboundedSender<AgentEvent>, &mut bool)) -> (Vec<AgentEvent>, bool) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut produced = false;
        f(&tx, &mut produced);
        drop(tx);
        let mut out = vec![];
        // The unbounded receiver is async; pull synchronously via try_recv.
        let mut rx = rx;
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        (out, produced)
    }

    #[test]
    fn agent_message_becomes_text() {
        let item = json!({ "id": "i1", "type": "agent_message", "text": "hi there" });
        let (evs, produced) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(produced);
        assert!(matches!(&evs[..], [AgentEvent::Text(t)] if t == "hi there"));
    }

    #[test]
    fn command_execution_emits_call_then_result() {
        let started = json!({ "id": "c1", "type": "command_execution", "command": "bash -lc ls", "status": "in_progress" });
        let (evs, _) = drain(|tx, p| handle_item("item.started", &started, tx, p));
        match &evs[..] {
            [AgentEvent::ToolCall { id, name, input }] => {
                assert_eq!(id, "c1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "bash -lc ls");
            }
            _ => panic!("expected one ToolCall, got {evs:?}"),
        }
        let done = json!({ "id": "c1", "type": "command_execution", "aggregated_output": "docs\nsrc\n", "exit_code": 0, "status": "completed" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &done, tx, p));
        assert!(
            matches!(&evs[..], [AgentEvent::ToolResult { id, text }] if id == "c1" && text == "docs\nsrc\n")
        );

        let failed = json!({ "id": "c2", "type": "command_execution", "aggregated_output": "nope", "exit_code": 2, "status": "failed" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &failed, tx, p));
        assert!(
            matches!(&evs[..], [AgentEvent::ToolResult { text, .. }] if text == "nope\n(exit 2)")
        );
    }

    #[test]
    fn file_change_becomes_apply_patch_calls() {
        let item = json!({
            "id": "f1", "type": "file_change", "status": "completed",
            "changes": [ {"path":"a.rs","kind":"update"}, {"path":"b.rs","kind":"add"} ],
        });
        let (evs, produced) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(produced);
        match &evs[..] {
            [AgentEvent::ToolCall {
                id: i0,
                name: n0,
                input: in0,
            }, AgentEvent::ToolCall {
                id: i1,
                name: n1,
                input: in1,
            }] => {
                assert_eq!((i0.as_str(), n0.as_str()), ("f1-0", "apply_patch"));
                assert_eq!(in0["file_path"], "a.rs (update)");
                assert_eq!((i1.as_str(), n1.as_str()), ("f1-1", "apply_patch"));
                assert_eq!(in1["file_path"], "b.rs (add)");
            }
            _ => panic!("expected two apply_patch ToolCalls, got {evs:?}"),
        }
    }

    #[test]
    fn todo_list_maps_to_todowrite_shape() {
        let item = json!({
            "id": "t1", "type": "todo_list", "status": "completed",
            "items": [ {"text":"do a","completed":true}, {"text":"do b","completed":false} ],
        });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        match &evs[..] {
            [AgentEvent::ToolCall { name, input, .. }] => {
                assert_eq!(name, "todowrite");
                let todos = input["todos"].as_array().unwrap();
                assert_eq!(todos[0]["content"], "do a");
                assert_eq!(todos[0]["status"], "completed");
                assert_eq!(todos[1]["status"], "pending");
            }
            _ => panic!("expected one todowrite ToolCall, got {evs:?}"),
        }
    }

    #[test]
    fn reasoning_becomes_thinking() {
        let item = json!({ "id": "r1", "type": "reasoning", "text": "thinking hard" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(matches!(&evs[..], [AgentEvent::Thinking(t)] if t == "thinking hard"));
    }
}
