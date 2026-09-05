//! The agent→chat transcript layer.
//!
//! An agent turn becomes a chat message in three steps, and this crate owns all
//! three so that every producer's turn LOOKS the same in the transcript:
//!
//!   1. [`AgentEvent`] — the normalized vocabulary a turn speaks. Claude Code's
//!      stream-json, Codex's app-server protocol and a DeepSeek tool-calling
//!      loop inside the api all parse down to it.
//!   2. [`render`] — one event → one markdoc card (`{% mafold/tool %}`,
//!      `{% mafold/diff %}`, `{% mafold/bash %}`, `{% mafold/thinking %}`,
//!      `{% mafold/result %}`, …). These are published cards, so every client
//!      already renders them; a producer only has to emit the text.
//!   3. [`Transcript`] — the state machine that interleaves narration with tool
//!      activity, collapses consecutive tool cards into one `{% mafold/run %}`
//!      group, and lands each result inside the card of the call that made it.
//!
//! Who uses it: the self-hosted daemon (`mafold-cli`, over its harnesses) and
//! the api's server-side brains (`mafold-api`, e.g. @mafold's app builder).
//! They differ only in transport — snapshots vs. append-only deltas — which is
//! the one axis [`Transcript`] deliberately leaves to the caller.

pub mod event;
pub mod lint;
pub mod render;
pub mod transcript;

pub use event::AgentEvent;
pub use transcript::{Advance, Boundary, Qualify, Transcript};
