//! The normalized event vocabulary an agent turn speaks.
//!
//! Anything that drives an agent — the self-hosted daemon's harnesses (Claude
//! Code, Codex, Kimi Code) or a brain running inside the api — parses its own
//! native output down to this one shape. [`crate::render`] turns these into
//! chat cards, so what a turn LOOKS like in the transcript is decided in
//! exactly one place, no matter who produced it.

use serde_json::Value;

/// One normalized event from an agent turn. Producer-specific output formats
/// (Claude Code stream-json, DeepSeek tool calls, …) are parsed down to this
/// common shape.
/// (`Session` is unused by Claude Code — it returns its session in the daemon's
/// `TurnOutcome` — but other producers may stream it; kept as common API
/// surface.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The producer's resumable session id for this conversation (first seen).
    Session(String),
    /// A chunk of streamed assistant text.
    Text(String),
    /// The producer ABANDONED the assistant message it was streaming and is
    /// starting that message over — an API connection dropped mid-response and
    /// the SDK retries by re-streaming from the first token, not by resuming.
    /// Carries the text of the abandoned attempt so the transcript can un-say
    /// exactly that and nothing else.
    ///
    /// Without it the reply grows one duplicate copy of the opening line per
    /// retry. On 2026-08-15 three attempts spliced into
    /// `Now theNow the RN twin — first `TNow the RN twin — first `TextArea`…`,
    /// which left an ODD number of backticks and swallowed the 19.5k of tool
    /// cards behind it into the bubble as raw text.
    TextRewind(String),
    /// A tool / function call the agent made.
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a tool call (correlated by `id`).
    ToolResult { id: String, text: String },
    /// A thinking / chain-of-thought block (collapsed in the UI).
    Thinking(String),
    /// An image the agent PRODUCED this turn, as a path on the producer's
    /// machine. The render loop uploads it and attaches it to the reply, so it
    /// arrives in the same bubble as the text — identical to a person sending a
    /// photo.
    ///
    /// Producer-agnostic on purpose: each one maps its own native image output
    /// onto this event (Codex's `image_gen` writes to
    /// `$CODEX_HOME/generated_images/…`), exactly as each maps its own
    /// file-edit shape onto `ToolCall`. Nothing downstream knows which model
    /// drew the picture.
    Image { path: std::path::PathBuf },
    /// Streaming activity that is NOT rendered as content (thinking / tool-arg
    /// deltas, usage updates): `chars` of raw stream progress plus, when the
    /// producer knows it, the REAL cumulative output-token count for the turn.
    /// Drives the `{% generating %}` card's live heartbeat (beat / elapsed /
    /// tokens) so the indicator reflects actual model progress — never the
    /// transcript.
    Pulse { chars: u64, tokens: Option<u64> },
    /// The producer compacted its OWN context part-way through the turn (Claude
    /// Code's auto-compact). It takes minutes and produces no stream output
    /// while it runs, so relaying it is what keeps a long reply from reading as
    /// a hang. `pre_tokens` is the context size that was compacted, when the
    /// producer reports it.
    Compacted { pre_tokens: Option<u64> },
    /// The producer reported a usage limit that is NOT in the healthy state —
    /// the quota is exhausted or restricted. Producers emit this ONLY for the
    /// non-healthy states; a limit that's fine is not news and must not be
    /// relayed into every reply.
    RateLimited { kind: String, resets_at: Option<i64> },
    /// End-of-turn summary.
    Done {
        duration_ms: Option<f64>,
        cost_usd: Option<f64>,
        tokens: Option<u64>,
    },
    /// (driver-internal) The user answered the pending interactive ask — the
    /// daemon's agent loop emits this when it routes a reply into `ask_file`,
    /// and the renderer stamps the answer into the open `{% ask %}` card so the
    /// message content itself records "answered" (survives reload, reaches
    /// every client). Harnesses never emit this.
    AskAnswered(String),
}
