//! The transcript state machine: a stream of [`AgentEvent`]s in, the reply's
//! evolving markdoc out.
//!
//! This is the part of "what an agent turn looks like in chat" that is NOT the
//! card formatting ([`crate::render`]) and NOT the transport: how narration and
//! tool activity interleave, which consecutive tool cards collapse into one
//! `{% mafold/run %}` group, and how a tool's RESULT finds its way back into
//! the card of the call that produced it.
//!
//! Two transports read it, and they read it differently — which is why the
//! state machine is shared and the reading is not:
//!
//!   * the self-hosted daemon pushes whole snapshots (`editDraft`, the Telegram
//!     draft model) because its harness streams every call first and every
//!     result after, out of order — a card must be able to change after it was
//!     painted. It reads [`Transcript::snapshot`].
//!   * a brain inside the api yields append-only deltas, because it executes
//!     its own tools synchronously and a group is already complete by the time
//!     it closes. It reads [`Transcript::take_committed`].
//!
//! Neither is a special case of the other, and neither gets its own renderer.

use std::collections::HashMap;

use crate::event::AgentEvent;
use crate::render::{self, GroupItem, ToolStep};

/// Narration is committed in batches this large so a snapshot isn't rewritten
/// per token. Matches the daemon's long-standing value.
const TEXT_BATCH: usize = 240;

/// What the caller should do about the event it just fed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Nothing about the CONTENT changed — a heartbeat, a session id, an image
    /// the caller uploads out-of-band. The caller may still want to push (to
    /// refresh a liveness indicator); the transcript has no opinion.
    Quiet,
    /// Content grew. A throttled push is enough: narration reads fine arriving
    /// in ~300ms batches.
    Streamed,
    /// Paint NOW, throttle bypassed. A tool call or its result is the "middle
    /// state" the transcript model promises — batching it behind a timer is
    /// exactly what makes a working agent look like it went quiet.
    Immediate,
    /// Terminal. Nothing more will come; [`Transcript::finish`] has the final
    /// content, and it no longer carries a generating indicator.
    Done,
}

/// How many bytes of pending narration are safe to commit right now. A
/// producer whose text may contain card tags uses this to avoid committing
/// half of one (the daemon's `cardtags::commit_boundary`).
pub type Boundary = fn(&str) -> usize;

/// Canonicalise committed content. The daemon splices the official namespace
/// into bare card tags here (`{% html %}` → `{% mafold/html %}`).
pub type Qualify = fn(&str) -> String;

fn commit_everything(buf: &str) -> usize {
    buf.len()
}

fn verbatim(full: &str) -> String {
    full.to_string()
}

pub struct Transcript {
    /// Content committed to the reply so far.
    full: String,
    /// Narration the model has produced but that isn't committed yet.
    buf: String,
    /// The pending consecutive tool cards → one `{% mafold/run %}`. Held as
    /// SLOTS, not as appended text: a call takes its slot immediately (so it
    /// paints right away) and holds it for its result, which is what lets the
    /// group read `call → its output` even when every call streams before
    /// every result.
    group: Vec<GroupItem>,
    /// tool_use_id → slot index in `group`.
    open: HashMap<String, usize>,
    /// Per-category tool counts driving the group's human summary.
    counts: HashMap<&'static str, usize>,
    /// tool_use_id → lowercased tool name, for the renderer.
    names: HashMap<String, String>,
    /// Cursor into `full` for [`Transcript::take_committed`].
    emitted: usize,
    boundary: Boundary,
    qualify: Qualify,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    /// A transcript whose text is committed verbatim — right for any producer
    /// that doesn't emit card tags in its narration.
    pub fn new() -> Self {
        Self::with_text_policy(commit_everything, verbatim)
    }

    /// A transcript that holds back a chunk ending mid-tag and canonicalises
    /// committed content. Both hooks are plain functions rather than a trait
    /// object: the only implementation that needs them is the daemon's, and it
    /// already has them as free functions.
    pub fn with_text_policy(boundary: Boundary, qualify: Qualify) -> Self {
        Self {
            full: String::new(),
            buf: String::new(),
            group: Vec::new(),
            open: HashMap::new(),
            counts: HashMap::new(),
            names: HashMap::new(),
            emitted: 0,
            boundary,
            qualify,
        }
    }

    /// Commit as much pending narration as the text policy allows. Called on
    /// every batch boundary and by the caller's idle tick, so narration keeps
    /// moving during a long silent stretch.
    pub fn flush_text(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let n = (self.boundary)(&self.buf);
        if n == 0 {
            return;
        }
        let tagged = self.buf[..n].contains("{%");
        self.full.push_str(&self.buf[..n]);
        self.buf.drain(..n);
        // Only re-canonicalise when a tag actually landed: `qualify` walks the
        // whole message, and most commits are plain prose.
        if tagged {
            self.full = (self.qualify)(&self.full);
        }
    }

    fn flush_text_all(&mut self) {
        if !self.buf.is_empty() {
            self.full.push_str(&self.buf);
            self.buf.clear();
        }
        self.full = (self.qualify)(&self.full);
    }

    fn close_group(&mut self) {
        if self.group.is_empty() {
            return;
        }
        let card = render::run_card(
            &render::run_summary(&self.counts),
            &render::render_group(&self.group),
        );
        self.full.push_str(&card);
        self.group.clear();
        // Slots are gone once committed — a result arriving after this takes
        // the orphan path instead of writing into a stale index.
        self.open.clear();
        self.counts.clear();
    }

    /// Commit everything pending: all narration, and the open tool group as a
    /// finished `{% mafold/run %}`. After this nothing is held back, which is
    /// what a caller needs before splicing its own markdoc in ([`push_raw`]) or
    /// before reading the final content.
    ///
    /// [`push_raw`]: Transcript::push_raw
    pub fn seal(&mut self) {
        self.flush_text_all();
        self.close_group();
    }

    /// Append markdoc the caller assembled itself — the daemon's
    /// `{% mafold/bgtasks %}` notice, or a one-line apology for something that
    /// failed out-of-band. Pending narration and the open group are committed
    /// first, so the insertion lands after everything already said instead of
    /// jumping ahead of it.
    ///
    /// Uses the partial text commit, not [`seal`]: this can fire mid-stream,
    /// and a chunk that ends inside a card tag must stay held back. A caller
    /// splicing at end-of-turn calls [`seal`] itself first.
    ///
    /// [`seal`]: Transcript::seal
    pub fn push_raw(&mut self, md: &str) {
        self.flush_text();
        self.close_group();
        self.full.push_str(md);
    }

    /// Stamp an answer into the pending `{% mafold/ask %}` card. False when
    /// there is no unanswered one.
    pub fn stamp_ask(&mut self, answer: &str) -> bool {
        render::stamp_ask_answered(&mut self.full, answer)
    }

    /// Feed one event.
    pub fn push(&mut self, ev: &AgentEvent) -> Advance {
        match ev {
            AgentEvent::Text(t) => {
                // Tools so far → one run card, BEFORE this narration: the
                // transcript is a time series, and text that arrived after a
                // tool call must not be printed above it.
                self.close_group();
                self.buf.push_str(t);
                if self.buf.len() >= TEXT_BATCH {
                    self.flush_text();
                }
                Advance::Streamed
            }
            // Liveness and out-of-band payloads: no content of their own.
            AgentEvent::Pulse { .. } | AgentEvent::Session(_) | AgentEvent::Image { .. } => {
                Advance::Quiet
            }
            // The interactive ask BLOCKS the turn — everything so far plus the
            // live card has to be on screen before anyone can answer it.
            AgentEvent::ToolCall { name, .. } if name.eq_ignore_ascii_case("AskUserQuestion") => {
                self.flush_text();
                self.close_group();
                if let Some(s) = render::render(ev, &mut self.names) {
                    self.full.push_str(&s);
                }
                Advance::Immediate
            }
            AgentEvent::AskAnswered(answer) => {
                if self.stamp_ask(answer) {
                    Advance::Immediate
                } else {
                    Advance::Quiet
                }
            }
            AgentEvent::Done { .. } => {
                self.seal();
                if let Some(s) = render::render(ev, &mut self.names) {
                    self.full.push_str(&s); // {% mafold/result %}
                }
                Advance::Done
            }
            // tool / diff / bash result / thinking → into the current group.
            _ => {
                self.flush_text(); // narration before this group goes out first
                if let Some(k) = render::tool_kind(ev) {
                    *self.counts.entry(k).or_insert(0) += 1;
                }
                match ev {
                    // The call takes a slot NOW — it paints this push, with its
                    // output still pending — and holds it for its result.
                    AgentEvent::ToolCall { id, name, input } => {
                        self.names.insert(id.clone(), name.to_lowercase());
                        self.open.insert(id.clone(), self.group.len());
                        self.group.push(GroupItem::Step(ToolStep::new(name, input)));
                    }
                    // Into its call's slot. No slot (the group was already
                    // committed) → fall back to a standalone output card: a
                    // result with nowhere to go is still a result the user needs.
                    AgentEvent::ToolResult { id, text } => match self.open.get(id) {
                        Some(&i) => {
                            if let Some(GroupItem::Step(s)) = self.group.get_mut(i) {
                                s.land(text);
                            }
                        }
                        None => {
                            if let Some(s) = render::render(ev, &mut self.names) {
                                self.group.push(GroupItem::Card(s));
                            }
                        }
                    },
                    _ => {
                        if let Some(s) = render::render(ev, &mut self.names) {
                            self.group.push(GroupItem::Card(s));
                        }
                    }
                }
                Advance::Immediate
            }
        }
    }

    /// The running content for a snapshot transport: everything committed plus
    /// the still-open tool group rendered LIVE, so its summary ticks
    /// "Read 1 file" → "Read 2 files" and its cards stream out one by one
    /// instead of arriving as a finished block when the group closes.
    ///
    /// Re-rendering the open group on every push is free — the group is a pure
    /// function of its items, and committing it later produces byte-identical
    /// text, so the transition is invisible.
    pub fn snapshot(&self) -> String {
        if self.group.is_empty() {
            return self.full.clone();
        }
        format!(
            "{}{}",
            self.full,
            render::run_card(
                &render::run_summary(&self.counts),
                &render::render_group(&self.group),
            )
        )
    }

    /// Content committed since the last call — the append-only view, for a
    /// transport that streams deltas instead of pushing snapshots.
    ///
    /// Only COMMITTED content is returned: an open tool group stays pending
    /// until it closes, so a consumer that can never un-say anything is never
    /// shown a card that is going to change. Pair it with the default text
    /// policy — a policy that REWRITES already-committed content has nothing
    /// an append-only consumer can do about it, and the cursor just resyncs to
    /// the new end.
    pub fn take_committed(&mut self) -> String {
        if self.emitted > self.full.len() {
            self.emitted = self.full.len();
        }
        while self.emitted < self.full.len() && !self.full.is_char_boundary(self.emitted) {
            self.emitted += 1;
        }
        let out = self.full[self.emitted..].to_string();
        self.emitted = self.full.len();
        out
    }

    /// Everything committed so far, without closing anything.
    pub fn content(&self) -> &str {
        &self.full
    }

    /// Terminal: commit everything pending and hand back the final markdoc.
    ///
    /// After a `Done` this is just a read. Its real job is the stream that ends
    /// WITHOUT one — an error, a kill — where the pending narration and the
    /// half-built group are still the best record of what happened.
    pub fn finish(&mut self) -> String {
        self.seal();
        self.full.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, input: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolCall { id: id.into(), name: name.into(), input }
    }
    fn result(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolResult { id: id.into(), text: text.into() }
    }

    /// The whole point of the slot model: a call and its result are ONE card,
    /// even though they arrive as two events with other events in between.
    #[test]
    fn a_call_and_its_result_are_one_card() {
        let mut t = Transcript::new();
        assert_eq!(t.push(&call("a", "Bash", json!({"command":"pnpm test"}))), Advance::Immediate);
        assert_eq!(t.push(&result("a", "42 passed")), Advance::Immediate);
        let md = t.finish();
        assert_eq!(md.matches("{% mafold/tool").count(), 1, "{md}");
        assert!(md.contains("pnpm test") && md.contains("42 passed"), "{md}");
        assert!(md.contains("{% mafold/run summary=\"Ran 1 shell command\""), "{md}");
    }

    /// Narration that arrives AFTER a tool call must print after it. A
    /// transcript is a time series; reordering it is a lie about what happened.
    #[test]
    fn narration_after_a_group_stays_after_it() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("Looking at the tests.".into()));
        t.push(&call("a", "Read", json!({"file_path": "a.rs"})));
        t.push(&result("a", "x\ny"));
        t.push(&AgentEvent::Text("They pass.".into()));
        let md = t.finish();
        let intro = md.find("Looking at").expect("intro");
        let group = md.find("{% mafold/run").expect("group");
        let outro = md.find("They pass").expect("outro");
        assert!(intro < group && group < outro, "{md}");
    }

    /// An append-only consumer must never be handed a card that will change:
    /// the open group is withheld until it closes.
    #[test]
    fn take_committed_withholds_an_open_group() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("Building.".into()));
        t.flush_text();
        assert_eq!(t.take_committed(), "Building.");

        t.push(&call("a", "Bash", json!({"command":"make"})));
        assert_eq!(t.take_committed(), "", "an open group must not be emitted");

        t.push(&result("a", "ok"));
        assert_eq!(t.take_committed(), "", "still open");

        // Narration closes the group.
        t.push(&AgentEvent::Text("Done.".into()));
        let out = t.take_committed();
        assert!(out.contains("{% mafold/run"), "{out}");
        assert!(out.contains("make") && out.contains("ok"), "{out}");
        // …and the group is never handed over twice.
        let rest = t.finish();
        assert_eq!(rest.matches("{% mafold/run").count(), 1, "{rest}");
    }

    /// Successive `take_committed` calls PARTITION the content: concatenating
    /// every chunk reproduces the final message exactly — no byte dropped, none
    /// emitted twice. This is the invariant the append-only transport rests on.
    #[test]
    fn take_committed_partitions_the_content() {
        let mut t = Transcript::new();
        let mut seen = String::new();

        t.push(&AgentEvent::Text("one ".into()));
        t.flush_text();
        seen.push_str(&t.take_committed());

        t.push(&call("a", "Read", json!({"file_path": "b.rs"})));
        t.push(&result("a", "1\n2\n3"));
        t.push(&AgentEvent::Text("two".into())); // closes the group
        seen.push_str(&t.take_committed());

        t.push(&AgentEvent::Done { duration_ms: Some(500.0), cost_usd: None, tokens: None });
        seen.push_str(&t.take_committed());

        assert_eq!(seen, t.finish(), "chunks must reassemble into the final message");
        assert_eq!(t.take_committed(), "", "nothing left over");
    }

    /// `finish` is a read, not a drain — it returns the WHOLE message even
    /// after chunks have been taken.
    #[test]
    fn finish_returns_everything_regardless_of_takes() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("hello".into()));
        t.flush_text();
        assert_eq!(t.take_committed(), "hello");
        assert_eq!(t.finish(), "hello");
    }

    /// Multi-byte content must not panic the cursor.
    #[test]
    fn take_committed_is_utf8_safe() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("正在生成并部署 app…".into()));
        t.flush_text();
        assert_eq!(t.take_committed(), "正在生成并部署 app…");
        assert_eq!(t.take_committed(), "");
    }

    /// `Done` closes everything and stamps the result card last.
    #[test]
    fn done_seals_and_stamps_the_result() {
        let mut t = Transcript::new();
        t.push(&call("a", "Bash", json!({"command":"ls"})));
        t.push(&result("a", "src"));
        let adv = t.push(&AgentEvent::Done {
            duration_ms: Some(18_300.0),
            cost_usd: None,
            tokens: Some(4200),
        });
        assert_eq!(adv, Advance::Done);
        let md = t.finish();
        let group = md.find("{% mafold/run").expect("group");
        let res = md.find("{% mafold/result").expect("result");
        assert!(group < res, "the result stamp goes last: {md}");
        assert!(md.contains("duration=\"18.3s\"") && md.contains("tokens=\"4.2k\""), "{md}");
    }

    /// Spliced markdoc lands after everything already said — and, at
    /// end-of-turn, before the result stamp.
    #[test]
    fn push_raw_lands_in_order() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("Kicked off a watcher.".into()));
        t.push(&call("a", "Bash", json!({"command":"tail -f log"})));
        t.seal();
        t.push_raw("\n{% mafold/bgtasks %}\ntail -f log\n{% /mafold/bgtasks %}\n");
        t.push(&AgentEvent::Done { duration_ms: Some(1000.0), cost_usd: None, tokens: None });
        let md = t.finish();
        let group = md.find("{% mafold/run").expect("group");
        let bg = md.find("{% mafold/bgtasks").expect("bgtasks");
        let res = md.find("{% mafold/result").expect("result");
        assert!(group < bg && bg < res, "{md}");
    }

    /// A result whose group already closed still shows up — as a standalone
    /// card, never dropped.
    #[test]
    fn an_orphan_result_is_not_lost() {
        let mut t = Transcript::new();
        t.push(&call("a", "Bash", json!({"command":"echo hi"})));
        t.push(&AgentEvent::Text("meanwhile…".into())); // closes the group
        t.push(&result("a", "hi"));
        let md = t.finish();
        assert!(md.contains("{% mafold/bash %}"), "orphan bash output card missing: {md}");
        assert!(md.contains("hi"), "{md}");
    }

    /// The text policy is honoured: a producer that holds back a half tag sees
    /// it stay in the buffer, and the qualifier runs on what lands.
    #[test]
    fn the_text_policy_holds_back_and_qualifies() {
        fn boundary(buf: &str) -> usize {
            // Hold back anything from an unclosed `{%` onward.
            match buf.rfind("{%") {
                Some(i) if !buf[i..].contains("%}") => i,
                _ => buf.len(),
            }
        }
        fn qualify(full: &str) -> String {
            full.replace("{% html %}", "{% mafold/html %}") // LINT-IGNORE
        }
        let mut t = Transcript::with_text_policy(boundary, qualify);
        t.push(&AgentEvent::Text("see this {% ht".into())); // LINT-IGNORE
        t.flush_text();
        assert_eq!(t.content(), "see this ", "half a tag must not commit");
        t.push(&AgentEvent::Text("ml %} done".into()));
        let md = t.finish();
        assert!(md.contains("{% mafold/html %}"), "not qualified: {md}");
        assert!(!md.contains("see this {% html"), "{md}"); // LINT-IGNORE
    }

    /// The snapshot shows the open group live; committing it later changes
    /// nothing visible.
    #[test]
    fn the_snapshot_shows_the_open_group_and_commits_identically() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("Checking.".into()));
        t.flush_text();
        t.push(&call("a", "Read", json!({"file_path": "a.rs"})));
        let live = t.snapshot();
        assert!(live.contains("{% mafold/run summary=\"Read 1 file\""), "{live}");
        t.push(&call("b", "Read", json!({"file_path": "b.rs"})));
        let live2 = t.snapshot();
        assert!(live2.contains("summary=\"Read 2 files\""), "summary must tick: {live2}");
        let committed = t.finish();
        assert_eq!(committed, live2, "committing an open group is visually seamless");
    }

    /// Liveness events carry no content — they must not disturb the transcript.
    #[test]
    fn heartbeats_are_content_free() {
        let mut t = Transcript::new();
        t.push(&AgentEvent::Text("hi".into()));
        t.flush_text();
        let before = t.content().to_string();
        assert_eq!(t.push(&AgentEvent::Pulse { chars: 120, tokens: Some(9) }), Advance::Quiet);
        assert_eq!(t.push(&AgentEvent::Session("sess-1".into())), Advance::Quiet);
        assert_eq!(
            t.push(&AgentEvent::Image { path: std::path::PathBuf::from("/tmp/x.png") }),
            Advance::Quiet
        );
        assert_eq!(t.content(), before);
    }
}
