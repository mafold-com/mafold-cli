//! Harness abstraction — the pluggable coding-agent backend a daemon drives.
//!
//! Today the only harness is **Claude Code**; planned: `opencode`, `codex`,
//! `openclaw`. Each harness knows how to invoke its CLI headlessly and normalize
//! that CLI's output into [`AgentEvent`]s. The renderer (`crate::render`) turns
//! those events into chat text + cards, so card rendering is identical across
//! harnesses — a new harness only has to emit the common event stream.
//!
//! A `Daemon` (one bot presence) is `(token + workdir + harness + model)`; the
//! supervisor runs many daemons, one process per bot.

pub mod claude_code;

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc::UnboundedSender, Notify};

use crate::client::Client;

/// PIDs of harness child processes (claude runs) currently IN FLIGHT. The
/// daemon's shutdown handler kills exactly these — never the process group:
/// legitimate background tasks the agent left running (run_in_background
/// shells) share the daemon's pgroup and MUST survive a daemon restart
/// (0.9.46's group kill wrongly took them down — the 2026-07-19 regression).
pub fn live_children() -> &'static Mutex<HashSet<u32>> {
    static LIVE: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// RAII registration of a harness child pid in [`live_children`] — deregisters
/// on drop, so every exit path (clean, error, panic) cleans up.
pub struct ChildGuard(Option<u32>);
impl ChildGuard {
    pub fn new(pid: Option<u32>) -> Self {
        if let Some(p) = pid {
            live_children().lock().unwrap().insert(p);
        }
        Self(pid)
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            live_children().lock().unwrap().remove(&p);
        }
    }
}

/// One normalized event from a harness turn. Harness-specific output formats
/// (Claude Code stream-json, etc.) are parsed down to this common shape.
/// (`Session` is unused by Claude Code — it returns its session in `TurnOutcome`
/// — but other harnesses may stream it; kept as common API surface.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The harness's resumable session id for this conversation (first seen).
    Session(String),
    /// A chunk of streamed assistant text.
    Text(String),
    /// A tool / function call the agent made.
    ToolCall { id: String, name: String, input: Value },
    /// The result of a tool call (correlated by `id`).
    ToolResult { id: String, text: String },
    /// A thinking / chain-of-thought block (collapsed in the UI).
    Thinking(String),
    /// Streaming activity that is NOT rendered as content (thinking / tool-arg
    /// deltas, usage updates): `chars` of raw stream progress plus, when the
    /// harness knows it, the REAL cumulative output-token count for the turn.
    /// Drives the `{% generating %}` card's live heartbeat (beat / elapsed /
    /// tokens) so the indicator reflects actual model progress — never the
    /// transcript.
    Pulse { chars: u64, tokens: Option<u64> },
    /// End-of-turn summary.
    Done { duration_ms: Option<f64>, cost_usd: Option<f64>, tokens: Option<u64> },
    /// (daemon-internal) The user answered the pending interactive ask — the
    /// agent loop emits this when it routes a reply into `ask_file`, and the
    /// renderer stamps the answer into the open `{% ask %}` card so the message
    /// content itself records "answered" (survives reload, reaches every
    /// client). Harnesses never emit this.
    AskAnswered(String),
}

/// One turn to run against a harness.
pub struct Turn {
    pub prompt: String,
    /// The conversation id this turn runs in — exported to the agent's process
    /// env as `MAFOLD_CONV` so `mafold room …` (the room skill) targets it.
    pub conv: String,
    pub workdir: String,
    /// The harness's prior session id for this conversation, to resume context.
    pub session: Option<String>,
    /// Per-chat model override (`/model`), or None for the harness default.
    pub model: Option<String>,
    /// Reasoning-effort level (`low`/`medium`/`high`/`xhigh`/`max`), or None for
    /// the harness default. Maps to Claude Code's `--effort`.
    pub effort: Option<String>,
    /// Extended-thinking budget in tokens (`/think`), or None for the harness
    /// default (off). Maps to Claude Code's `MAX_THINKING_TOKENS`.
    pub thinking: Option<u32>,
    /// Fires when `/stop` is invoked — the harness must kill its child and stop.
    pub cancel: Arc<Notify>,
    /// Extra system-prompt context (the daemon's mafold preamble: identity, the
    /// current conversation, embeddable cards). Appended to the agent's own
    /// system prompt; None = a pure, mafold-unaware agent.
    pub system: Option<String>,
    /// Per-turn file the AskUserQuestion PreToolUse hook waits on: the daemon
    /// writes the user's chat-card answer here, the hook reads it and feeds it
    /// back to the model. None = interactive ask unsupported for this harness.
    pub ask_file: Option<String>,
}

/// Outcome of a turn.
#[derive(Default)]
pub struct TurnOutcome {
    /// Any content (text or a tool event) was produced.
    pub produced: bool,
    /// The turn was interrupted by `/stop`.
    pub stopped: bool,
    /// The (possibly new) session id to persist for this conversation.
    pub session: Option<String>,
    /// Set when the agent ended on an API / execution error (an `is_error`
    /// result, or a fatal error event mid-stream) rather than completing — the
    /// specific reason to surface to the user. The turn stops cleanly and the
    /// session is still persisted, so a retry resumes with context.
    pub error: Option<String>,
}

/// Where a slash command lands when not a daemon control command.
/// (`Handled` is for harnesses whose command posts its own messages; Claude
/// Code only returns `Reply`/`Forward` today.)
#[allow(dead_code)]
pub enum CommandOutcome {
    /// Handled locally — send this markdown reply.
    Reply(String),
    /// Handled locally; the harness already posted its own messages.
    Handled,
    /// Not a harness command — forward the raw text to the harness as a prompt.
    Forward,
}

/// A pluggable coding-agent backend.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Stable id — `"claude-code"`, `"opencode"`, `"codex"`, `"openclaw"`.
    fn id(&self) -> &'static str;

    /// Is the harness CLI installed / runnable on this machine?
    fn available(&self) -> bool;

    /// Run one turn, pushing normalized events into `sink` as they arrive.
    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> anyhow::Result<TurnOutcome>;

    /// Discover the harness's slash-commands / skills for the bot's `/` menu
    /// (a JSON array of `{command, description, arg_hint?}`).
    fn discover(&self, workdir: &str) -> Value;

    /// Try to handle a slash command locally (config dumps, /login, /stats…),
    /// or return `Forward` to run it as a prompt.
    async fn command(&self, client: &Client, chat_id: &str, name: &str, arg: &str, workdir: &str) -> CommandOutcome;

    /// One-line status (e.g. auth account) appended to `/status`. Empty = none.
    async fn status_line(&self) -> String {
        String::new()
    }
}

/// Every harness id the CLI knows about (installed or not) — for menus / docs.
/// (Used by the supervisor to list/validate harnesses — Phase 2.)
#[allow(dead_code)]
pub const KNOWN: &[&str] = &["claude-code", "opencode", "codex", "openclaw"];

/// The default when a bot doesn't specify one.
#[allow(dead_code)]
pub const DEFAULT: &str = "claude-code";

/// Resolve a harness by id (case/alias tolerant). Unknown ids fall back to the
/// default so a misconfigured bot still runs Claude Code.
pub fn select(id: &str) -> Arc<dyn Harness> {
    match id.trim().to_lowercase().as_str() {
        "claude-code" | "claude" | "claudecode" | "" => Arc::new(claude_code::ClaudeCode),
        // opencode / codex / openclaw plug in here as they're implemented.
        _ => Arc::new(claude_code::ClaudeCode),
    }
}

/// Probe which known harnesses are installed on THIS machine (their CLI is on
/// PATH) — the control plane's capability report for New-Bot recommendation.
/// Returns `(id, available)`. Extend `BINS` as more harness impls land.
pub fn probe() -> Vec<(&'static str, bool)> {
    // (harness id, the CLI binary whose presence signals it's installed)
    const BINS: &[(&str, &str)] = &[
        ("claude-code", "claude"),
        ("opencode", "opencode"),
        ("codex", "codex"),
        ("openclaw", "openclaw"),
    ];
    BINS.iter().map(|(id, bin)| (*id, on_path(bin))).collect()
}

pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
        .unwrap_or(false)
}
